#!/usr/bin/env python3
import io
import json
import os
import stat
import sys
import tempfile
import time
from contextlib import contextmanager, redirect_stderr
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "adapters"))
import agent_meter_adapter as adapter

FIXTURES = Path(__file__).resolve().parent / "fixtures"

MUSE_FAKE = """#!/usr/bin/env python3
import json
import os
import sys

if sys.argv[1:3] != ["serve", "--no-session-log"]:
    sys.stderr.write("expected: serve --no-session-log\\n")
    raise SystemExit(2)
received = []
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    received.append(msg)
    if msg.get("method") == "usage/read":
        break
log_path = os.environ.get("MUSE_REQUESTS_LOG")
if log_path:
    with open(log_path, "w", encoding="utf-8") as handle:
        json.dump(received, handle)
fixture = os.environ["MUSE_FIXTURE"]
with open(fixture, encoding="utf-8") as handle:
    data = handle.read()
sys.stdout.write(data)
if data and not data.endswith("\\n"):
    sys.stdout.write("\\n")
sys.stdout.flush()
"""

# In-memory Muse: usage/read stays {} until a turn runs in this same process.
MUSE_MEMORY_FAKE = """#!/usr/bin/env python3
import json
import os
import sys

if sys.argv[1:3] != ["serve", "--no-session-log"]:
    sys.stderr.write("expected: serve --no-session-log\\n")
    raise SystemExit(2)

with open(os.environ["MUSE_USAGE_FIXTURE"], encoding="utf-8") as handle:
    rows = [json.loads(line) for line in handle if line.strip()]
usage = None
changed = None
for row in rows:
    if row.get("method") == "usage/changed":
        changed = row
    result = row.get("result")
    if isinstance(result, dict) and isinstance(result.get("usage"), dict):
        usage = result["usage"]
if usage is None:
    sys.stderr.write("fixture has no usage\\n")
    raise SystemExit(2)

turned = False
received = []
log_path = os.environ.get("MUSE_REQUESTS_LOG")


def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\\n")
    sys.stdout.flush()


def remember(msg):
    received.append(msg)
    if log_path:
        with open(log_path, "w", encoding="utf-8") as handle:
            json.dump(received, handle)


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    remember(msg)
    method = msg.get("method")
    if method == "initialized":
        continue
    if method == "initialize":
        emit({
            "jsonrpc": "2.0",
            "id": msg.get("id"),
            "result": {"serverInfo": {"name": "muse", "version": "1.4.7"}},
        })
        continue
    if method == "usage/read":
        if turned:
            emit({"jsonrpc": "2.0", "id": msg.get("id"), "result": {"usage": usage}})
        else:
            emit({"jsonrpc": "2.0", "id": msg.get("id"), "result": {}})
        continue
    if method == "session/start":
        emit({
            "jsonrpc": "2.0",
            "id": msg.get("id"),
            "result": {"session": {"sessionId": "sess-memory"}},
        })
        continue
    if method == "turn/start":
        turned = True
        emit({
            "jsonrpc": "2.0",
            "id": msg.get("id"),
            "result": {"status": "accepted", "turnId": "turn-memory"},
        })
        if changed is not None:
            emit(changed)
        else:
            emit({"jsonrpc": "2.0", "method": "turn/completed", "params": {"terminal": "completed"}})
        continue
"""

# Probe stand-in. MUSE_PROBE_MODE is error, exit, hang, empty, or ok.
# Turn count goes to MUSE_TURN_COUNTER. The muse-probe.json contents at the
# moment turn/start arrives are copied to MUSE_PROBE_SNAPSHOT.
MUSE_PROBE_FAKE = """#!/usr/bin/env python3
import json
import os
import sys
import time

def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\\n")
    sys.stdout.flush()

def count_turn():
    path = os.environ.get("MUSE_TURN_COUNTER")
    if not path:
        return
    current = 0
    if os.path.exists(path):
        raw = open(path, encoding="utf-8").read().strip()
        current = int(raw or "0")
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(str(current + 1))

def snapshot_probe_state():
    dest = os.environ.get("MUSE_PROBE_SNAPSHOT")
    if not dest:
        return
    source = os.path.join(os.environ.get("XDG_STATE_HOME", ""), "agent-meter", "muse-probe.json")
    try:
        data = open(source, encoding="utf-8").read()
    except OSError as exc:
        data = "unreadable:" + str(exc)
    with open(dest, "w", encoding="utf-8") as handle:
        handle.write(data)

if sys.argv[1:3] != ["serve", "--no-session-log"]:
    sys.stderr.write("expected: serve --no-session-log\\n")
    raise SystemExit(2)

mode = os.environ.get("MUSE_PROBE_MODE", "ok")
usage = None
if mode == "ok":
    with open(os.environ["MUSE_USAGE_FIXTURE"], encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            row = json.loads(line)
            result = row.get("result")
            if isinstance(result, dict) and isinstance(result.get("usage"), dict):
                usage = result["usage"]
    if usage is None:
        sys.stderr.write("fixture has no usage\\n")
        raise SystemExit(2)

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    try:
        msg = json.loads(raw)
    except json.JSONDecodeError:
        continue
    method = msg.get("method")
    if method == "initialized":
        continue
    if method == "initialize":
        emit({
            "jsonrpc": "2.0",
            "id": msg.get("id"),
            "result": {"serverInfo": {"name": "muse", "version": "1.4.7"}},
        })
        continue
    if method == "usage/read":
        if mode == "ok" and usage is not None and os.environ.get("MUSE_TURN_SEEN") == "1":
            emit({"jsonrpc": "2.0", "id": msg.get("id"), "result": {"usage": usage}})
        else:
            emit({"jsonrpc": "2.0", "id": msg.get("id"), "result": {}})
        continue
    if method == "session/start":
        emit({
            "jsonrpc": "2.0",
            "id": msg.get("id"),
            "result": {"session": {"sessionId": "sess-probe"}},
        })
        continue
    if method == "turn/start":
        count_turn()
        snapshot_probe_state()
        os.environ["MUSE_TURN_SEEN"] = "1"
        if mode == "exit":
            raise SystemExit(2)
        if mode == "error":
            emit({
                "jsonrpc": "2.0",
                "id": msg.get("id"),
                "error": {"code": -32000, "message": "probe rejected"},
            })
            continue
        if mode == "hang":
            time.sleep(30)
            continue
        emit({"jsonrpc": "2.0", "method": "turn/completed", "params": {"terminal": "completed"}})
        continue
"""

MUSE_UNAVAILABLE_REASON = (
    "Muse keeps subscription usage only in the memory of the process that made a model call "
    "and writes none to disk; enable the opt-in probe (AGENT_METER_MUSE_PROBE=1 or "
    "~/.config/agent-meter/muse.json) to let agent-meter make one minimal Muse call per interval"
)


@contextmanager
def patched_env(updates: dict[str, str | None]):
    previous = {key: os.environ.get(key) for key in updates}
    try:
        for key, value in updates.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
        yield
    finally:
        for key, value in previous.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


def _jsonl(name: str) -> list:
    rows = []
    for line in (FIXTURES / name).read_text(encoding="utf-8").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows


def _write_executable(path: Path, text: str) -> None:
    path.write_text(text, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IEXEC)


def _observed_muse_usage() -> dict:
    for row in _jsonl("muse-usage-observed.jsonl"):
        result = row.get("result")
        if isinstance(result, dict) and isinstance(result.get("usage"), dict):
            return result["usage"]
    raise AssertionError("muse-usage-observed.jsonl has no usage result")


def _ms(value: int) -> adapter.dt.datetime:
    return adapter.dt.datetime.fromtimestamp(value / 1000, tz=adapter.dt.UTC)


def test_reset_calendar_and_timezone() -> None:
    previous = os.environ.get("TZ")
    try:
        os.environ["TZ"] = "America/New_York"
        time.tzset()
        stamp, label = adapter.reset_label("2026-09-16T21:52:00Z")
        assert stamp == "2026-09-16T21:52:00Z"
        assert label == "Wed, Sep 16, 2026 · 5:52 PM EDT"
        assert adapter.reset_label("2027-01-01T02:00:00Z")[1] == "Thu, Dec 31, 2026 · 9:00 PM EST"
        assert adapter.reset_label("2026-11-01T05:30:00Z")[1].endswith("1:30 AM EDT")
        assert adapter.reset_label("2026-11-01T06:30:00Z")[1].endswith("1:30 AM EST")
        assert adapter.reset_label(None) == (None, None)
        assert adapter.reset_label("invalid") == (None, None)
    finally:
        if previous is None:
            os.environ.pop("TZ", None)
        else:
            os.environ["TZ"] = previous
        time.tzset()


def test_copilot_reset_validity() -> None:
    now = adapter.dt.datetime(2026, 9, 11, tzinfo=adapter.dt.UTC)
    for reset in (None, "invalid", "2026-09-10T00:00:00Z", "2026-09-11T00:00:00Z", "2026-10-01T00:00:00Z"):
        payload = {"quotaSnapshots": {"premium_interactions": {"remainingPercentage": 55, "resetDate": reset}}}
        window = adapter.normalize_copilot_quota(payload, now=now)["windows"][0]
        assert window["remaining_percent"] == 55
        if reset == "2026-10-01T00:00:00Z":
            assert window["resets_at"] == reset
            assert "2026" in window["reset_label"]
        else:
            assert "resets_at" not in window
            assert window["reset_label"] == "Reset unknown"


# ---------------------------------------------------------------------------
# normalize_codex
# ---------------------------------------------------------------------------

def test_codex() -> None:
    state = adapter.normalize_codex({
        "rateLimits": {
            "planType": "pro",
            "primary": {"usedPercent": 28, "windowDurationMins": 300, "resetsAt": 1_788_000_000},
            "secondary": {"usedPercent": 49, "windowDurationMins": 10_080, "resetsAt": 1_788_500_000},
        }
    })
    assert state["id"] == "codex"
    assert [window["remaining_percent"] for window in state["windows"]] == [72.0, 51.0]
    assert [window["label"] for window in state["windows"]] == ["5 hours", "Weekly"]


def test_codex_rateLimitsByLimitId() -> None:
    state = adapter.normalize_codex({
        "rateLimitsByLimitId": {
            "codex": {
                "planType": "team",
                "primary": {"usedPercent": 10, "windowDurationMins": 300, "resetsAt": 1_788_000_000},
            }
        }
    })
    assert state["id"] == "codex"
    assert state["windows"][0]["remaining_percent"] == 90.0


def test_codex_hung_app_server_times_out() -> None:
    with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as f:
        f.write(f"""#!/bin/sh
exec {sys.executable} -c "import time; time.sleep(60)"
""")
        mock_script = f.name
    os.chmod(mock_script, 0o755)
    old_bin = os.environ.get("AGENT_METER_CODEX_BIN")
    os.environ["AGENT_METER_CODEX_BIN"] = mock_script
    try:
        t0 = time.monotonic()
        state = adapter.codex(timeout=0.1)
        elapsed = time.monotonic() - t0
        assert elapsed < 1.0, f"Expected timeout under 1.0s, took {elapsed}s"
        assert state["id"] == "codex"
        assert state["status"] == "unavailable"
        assert "timed out" in state["detail"]
    finally:
        if old_bin is None:
            os.environ.pop("AGENT_METER_CODEX_BIN", None)
        else:
            os.environ["AGENT_METER_CODEX_BIN"] = old_bin
        os.remove(mock_script)


def test_codex_hung_after_init_times_out() -> None:
    with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as f:
        f.write(f"""#!/bin/sh
exec {sys.executable} -c '
import sys, json, time
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("id") == 1:
        print(json.dumps({{"id": 1, "result": {{}}}}), flush=True)
    elif msg.get("id") == 2:
        time.sleep(60)
'
""")
        mock_script = f.name
    os.chmod(mock_script, 0o755)
    old_bin = os.environ.get("AGENT_METER_CODEX_BIN")
    os.environ["AGENT_METER_CODEX_BIN"] = mock_script
    try:
        t0 = time.monotonic()
        state = adapter.codex(timeout=0.1)
        elapsed = time.monotonic() - t0
        assert elapsed < 1.0, f"Expected timeout under 1.0s, took {elapsed}s"
        assert state["id"] == "codex"
        assert state["status"] == "unavailable"
        assert "timed out" in state["detail"]
    finally:
        if old_bin is None:
            os.environ.pop("AGENT_METER_CODEX_BIN", None)
        else:
            os.environ["AGENT_METER_CODEX_BIN"] = old_bin
        os.remove(mock_script)


def test_codex_app_server_success() -> None:
    with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as f:
        f.write(f"""#!/bin/sh
exec {sys.executable} -c '
import sys, json
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("id") == 1:
        print(json.dumps({{"id": 1, "result": {{}}}}), flush=True)
    elif msg.get("id") == 2:
        print(json.dumps({{
            "id": 2,
            "result": {{
                "rateLimits": {{
                    "planType": "pro",
                    "primary": {{"usedPercent": 25, "windowDurationMins": 300, "resetsAt": 1788000000}},
                    "secondary": {{"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": 1788500000}}
                }}
            }}
        }}), flush=True)
'
""")
        mock_script = f.name
    os.chmod(mock_script, 0o755)
    old_bin = os.environ.get("AGENT_METER_CODEX_BIN")
    os.environ["AGENT_METER_CODEX_BIN"] = mock_script
    try:
        state = adapter.codex(timeout=2.0)
        assert state["id"] == "codex"
        assert state["status"] == "fresh"
        assert [window["remaining_percent"] for window in state["windows"]] == [75.0, 60.0]
    finally:
        if old_bin is None:
            os.environ.pop("AGENT_METER_CODEX_BIN", None)
        else:
            os.environ["AGENT_METER_CODEX_BIN"] = old_bin
        os.remove(mock_script)


# ---------------------------------------------------------------------------
# normalize_agy
# ---------------------------------------------------------------------------

def test_agy() -> None:
    state = adapter.normalize_agy({"quota_summary": {"groups": [{"display_name": "Gemini Models", "buckets": [
        {"window": "5h", "remaining_pct": 62.5, "reset_time": "2026-08-30T20:00:00+00:00"},
        {"window": "Weekly", "remaining_pct": 95.0, "reset_time": "2026-09-05T20:00:00+00:00"},
    ]}]}})
    assert state["id"] == "antigravity"
    assert len(state["windows"]) == 2
    assert state["windows"][0]["remaining_percent"] == 62.5


def test_agy_disabled_bucket_skipped() -> None:
    state = adapter.normalize_agy({"quota_summary": {"groups": [{"display_name": "Gemini Models", "buckets": [
        {"window": "5h", "remaining_pct": 50.0, "reset_time": "2026-08-30T20:00:00+00:00"},
        {"window": "Weekly", "remaining_pct": 80.0, "reset_time": "2026-09-05T20:00:00+00:00", "disabled": True},
    ]}]}})
    assert len(state["windows"]) == 1
    assert state["windows"][0]["remaining_percent"] == 50.0


def test_agy_provider_usage_command() -> None:
    state = adapter.normalize_agy({"command": {"name": "usage", "data": {"groups": [{
        "name": "Claude and GPT models",
        "buckets": [
            {"name": "Five Hour Limit Remaining", "window": "5h", "remaining_fraction": 0.397,
             "reset_time": "2026-08-30T20:00:00Z"},
            {"name": "Weekly Limit Remaining", "window": "weekly", "remaining_fraction": 0.434,
             "reset_time": "2026-09-04T00:50:56Z"},
        ],
    }]}}})
    assert [window["remaining_percent"] for window in state["windows"]] == [39.7, 43.4]
    assert state["detail"] == "Antigravity CLI /usage quota"


# ---------------------------------------------------------------------------
# normalize_claude
# ---------------------------------------------------------------------------

def test_claude_normalize() -> None:
    state = adapter.normalize_claude({"rate_limits": {
        "five_hour": {"used_percentage": 22, "resets_at": 1788000000},
        "seven_day": {"used_percentage": 41, "resets_at": "2026-09-05T20:00:00Z"},
    }})
    assert [w["remaining_percent"] for w in state["windows"]] == [78.0, 59.0]
    assert state["id"] == "claude"
    assert state["status"] == "fresh"


def test_claude_normalize_only_one_window() -> None:
    state = adapter.normalize_claude({"rate_limits": {
        "five_hour": {"used_percentage": 50, "resets_at": 1788000000},
    }})
    assert len(state["windows"]) == 1
    assert state["windows"][0]["remaining_percent"] == 50.0


def test_claude_normalize_missing_rate_limits() -> None:
    try:
        adapter.normalize_claude({})
        assert False, "Expected ValueError"
    except ValueError:
        pass


# ---------------------------------------------------------------------------
# claude() — state file
# ---------------------------------------------------------------------------

def test_claude_absent() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        os.environ["XDG_STATE_HOME"] = tmp
        try:
            result = adapter.claude()
        finally:
            del os.environ["XDG_STATE_HOME"]
    assert result["status"] == "unavailable"
    assert "agent-meter-claude-statusline" in result["detail"]
    assert result["id"] == "claude"


def test_claude_reads_normalized_state() -> None:
    normalized = {
        "id": "claude",
        "label": "Claude Code",
        "icon": "claude",
        "windows": [{"id": "five_hour", "label": "5 hours", "remaining_percent": 78.0}],
        "status": "fresh",
        "detail": "Claude Code status-line rate limits",
    }
    with tempfile.TemporaryDirectory() as tmp:
        state_dir = os.path.join(tmp, "agent-meter")
        os.makedirs(state_dir, exist_ok=True)
        path = os.path.join(state_dir, "claude.json")
        with open(path, "w") as f:
            json.dump(normalized, f)
        os.environ["XDG_STATE_HOME"] = tmp
        try:
            result = adapter.claude()
        finally:
            del os.environ["XDG_STATE_HOME"]
    assert result["status"] == "fresh"
    assert result["windows"][0]["remaining_percent"] == 78.0


def test_claude_reads_raw_statusline_json() -> None:
    raw = {"rate_limits": {
        "five_hour": {"used_percentage": 30, "resets_at": 4_102_444_800},
        "seven_day": {"used_percentage": 10, "resets_at": "2100-01-02T00:00:00Z"},
    }}
    with tempfile.TemporaryDirectory() as tmp:
        state_dir = os.path.join(tmp, "agent-meter")
        os.makedirs(state_dir, exist_ok=True)
        path = os.path.join(state_dir, "claude.json")
        with open(path, "w") as f:
            json.dump(raw, f)
        os.environ["XDG_STATE_HOME"] = tmp
        try:
            result = adapter.claude()
        finally:
            del os.environ["XDG_STATE_HOME"]
    assert result["status"] == "fresh"
    assert result["windows"][0]["remaining_percent"] == 70.0


def test_claude_marks_expired_state_stale() -> None:
    normalized = {
        "id": "claude",
        "label": "Claude Code",
        "icon": "claude",
        "windows": [{
            "id": "five_hour",
            "label": "5 hours",
            "remaining_percent": 80.0,
            "resets_at": "2020-01-01T00:00:00Z",
        }],
        "status": "fresh",
    }
    with tempfile.TemporaryDirectory() as tmp:
        state_dir = os.path.join(tmp, "agent-meter")
        os.makedirs(state_dir)
        with open(os.path.join(state_dir, "claude.json"), "w") as stream:
            json.dump(normalized, stream)
        os.environ["XDG_STATE_HOME"] = tmp
        try:
            result = adapter.claude()
        finally:
            del os.environ["XDG_STATE_HOME"]
    assert result["status"] == "stale"
    assert "cached" in result["detail"]


# ---------------------------------------------------------------------------
# normalize_copilot
# ---------------------------------------------------------------------------

def test_copilot_usage_items() -> None:
    payload = {
        "usageItems": [
            {"date": "2026-08-01T00:00:00Z", "product": "copilot", "sku": "Copilot AI Credits", "grossQuantity": 180.0},
            {"date": "2026-08-01T00:00:00Z", "product": "copilot", "sku": "Copilot Cloud Agent", "grossQuantity": 20.0},
            {"date": "2026-07-01T00:00:00Z", "product": "copilot", "sku": "Copilot AI Credits", "quantity": 150.0},
        ]
    }
    state = adapter.normalize_copilot(payload, allowance=300)
    assert state["status"] == "fresh"
    assert state["windows"][0]["remaining_percent"] == 33.3
    assert "200.0 of 300 AI credits" in state["detail"]


def test_copilot_with_allowance() -> None:
    state = adapter.normalize_copilot({"total_credits": 12}, allowance=100)
    assert state["windows"][0]["remaining_percent"] == 88.0
    assert state["status"] == "fresh"


def test_copilot_requires_explicit_allowance() -> None:
    state = adapter.normalize_copilot({"total_credits": 150})
    assert state["status"] == "unavailable"
    assert "150.0 AI credits used" in state["detail"]
    assert "copilot.json" in state["detail"]


def test_copilot_no_credits_raises() -> None:
    try:
        adapter.normalize_copilot({})
        assert False, "Expected ValueError"
    except ValueError:
        pass


def test_copilot_allowance_clamps_to_zero() -> None:
    state = adapter.normalize_copilot({"total_credits": 350}, allowance=300)
    assert state["windows"][0]["remaining_percent"] == 0.0


def test_copilot_total_premium_requests() -> None:
    state = adapter.normalize_copilot({"total_premium_requests": 50}, allowance=200)
    assert state["windows"][0]["remaining_percent"] == 75.0


def test_copilot_official_undated_response() -> None:
    state = adapter.normalize_copilot({"usageItems": [{
        "product": "Copilot AI Credits",
        "sku": "AI Credit",
        "unitType": "ai-credits",
        "grossQuantity": 75,
    }]}, allowance=300)
    assert state["windows"][0]["remaining_percent"] == 75.0


def test_copilot_sdk_quota() -> None:
    state = adapter.normalize_copilot_quota({"quotaSnapshots": {
        "premium_interactions": {
            "entitlementRequests": 300,
            "usedRequests": 42,
            "remainingPercentage": 86.0,
            "resetDate": "2026-09-01T00:00:00Z",
        },
        "chat": {"entitlementRequests": -1, "usedRequests": 0, "remainingPercentage": 100},
    }})
    assert len(state["windows"]) == 1
    assert state["windows"][0]["remaining_percent"] == 86.0
    assert "42 of 300 used" in state["detail"]


def test_grok_billing_snapshot() -> None:
    now = adapter.dt.datetime(2026, 8, 30, tzinfo=adapter.dt.UTC)
    state = adapter.normalize_grok([
        {"ts": "2026-08-29T20:00:00Z", "msg": "unrelated", "ctx": {}},
        {"ts": "2026-08-29T23:59:30Z", "msg": "billing: fetched credits config", "ctx": {"config": {
            "creditUsagePercent": 55.0,
            "billingPeriodEnd": "2026-09-05T01:45:30Z",
        }}},
    ], now=now)
    assert state["status"] == "fresh"
    # Grok's billing snapshot reports credit usage, while Agent Meter displays
    # the remaining percentage.
    assert state["windows"][0]["remaining_percent"] == 45.0


def test_omp_grok_usage_uses_shared_limit() -> None:
    state = adapter.normalize_omp_grok_usage({"reports": [{
        "provider": "xai-oauth",
        "limits": [
            {"id": "xai-oauth:credits:1w", "status": "warning", "window": {"resetsAt": 1788572730851},
             "amount": {"remaining": 4}},
            {"id": "xai-oauth:product:grokbuild:1w", "status": "ok", "window": {"resetsAt": 1788572730851},
             "amount": {"remainingFraction": 0.35}},
            {"id": "xai-oauth:product:grokvoice:1w", "status": "ok", "window": {"resetsAt": 1788572730851},
             "amount": {"remaining": 100}},
        ],
    }]})
    assert state["status"] == "fresh"
    assert state["windows"][0]["remaining_percent"] == 4.0
    assert len(state["windows"]) == 1
    assert state["windows"][0]["label"] == "Shared weekly"
    assert "OMP OAuth" in state["detail"]


def test_omp_grok_shared_statuses() -> None:
    for status, remaining in (("ok", 52), ("warning", 4), ("exhausted", 0)):
        payload = {"reports": [{"provider": "xai-oauth", "limits": [{
            "id": "xai-oauth:credits:1w", "status": status,
            "window": {"resetsAt": 1788572730851},
            "amount": {"remainingFraction": remaining / 100},
        }]}]}
        state = adapter.normalize_omp_grok_usage(payload)
        assert state["status"] == "fresh"
        assert state["windows"][0]["remaining_percent"] == remaining
    for identifier, status in (("xai-oauth:product:grokbuild:1w", "ok"), ("xai-oauth:credits:1w", "unknown")):
        payload["reports"][0]["limits"][0].update(id=identifier, status=status)
        try:
            adapter.normalize_omp_grok_usage(payload)
            assert False, "Do not substitute a product limit or unknown data for shared credits"
        except ValueError:
            pass


def test_grok_ended_period_is_stale_without_past_reset() -> None:
    """An ended billing period is returned stale, not dropped."""
    now = adapter.dt.datetime(2026, 8, 30, tzinfo=adapter.dt.UTC)
    state = adapter.normalize_grok([{"ts": "2026-08-20T00:00:00Z", "msg": "billing: fetched credits config", "ctx": {"config": {
        "creditUsagePercent": 47,
        "billingPeriodEnd": "2026-08-25T00:00:00Z",
    }}}], now=now)
    assert state["status"] == "stale"
    assert state["windows"][0]["remaining_percent"] == 53.0
    assert "resets_at" not in state["windows"][0]
    assert "billing period that ended" in state["detail"]
    assert "Grok Build" in state["detail"]


def test_grok_marks_old_snapshot_stale() -> None:
    now = adapter.dt.datetime(2026, 8, 30, tzinfo=adapter.dt.UTC)
    state = adapter.normalize_grok([{"ts": "2026-08-29T20:00:00Z", "msg": "billing: fetched credits config", "ctx": {"config": {
        "creditUsagePercent": 55,
        "billingPeriodEnd": "2026-09-05T00:00:00Z",
    }}}], now=now)
    assert state["status"] == "stale"
    assert "old" in state["detail"]


def test_grok_never_infers_quota_from_auth_file() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        grok_dir = os.path.join(tmp, ".grok")
        os.makedirs(grok_dir)
        with open(os.path.join(grok_dir, "auth.json"), "w") as stream:
            json.dump({"session": {"email": "private@example.invalid", "refresh_token": "secret"}}, stream)
        old_home = os.environ.get("HOME")
        os.environ["HOME"] = tmp
        try:
            state = adapter.grok()
        finally:
            if old_home is None:
                del os.environ["HOME"]
            else:
                os.environ["HOME"] = old_home
    assert state["status"] == "unavailable"
    assert "private@example.invalid" not in state["detail"]
    assert "secret" not in state["detail"]


def test_copilot_env_allowance() -> None:
    os.environ["AGENT_METER_COPILOT_ALLOWANCE"] = "500"
    try:
        state = adapter.normalize_copilot({"total_credits": 100}, allowance=500)
        assert state["windows"][0]["remaining_percent"] == 80.0
    finally:
        del os.environ["AGENT_METER_COPILOT_ALLOWANCE"]


# ---------------------------------------------------------------------------
# normalize_muse — real MSP SubscriptionUsage samples
# ---------------------------------------------------------------------------

def _muse_env(tmp: str, fixture: Path, requests_log: Path | None = None) -> dict[str, str | None]:
    fake = Path(tmp) / "muse"
    _write_executable(fake, MUSE_FAKE)
    env: dict[str, str | None] = {
        "AGENT_METER_MUSE_BIN": str(fake),
        "AGENT_METER_MUSE_TIMEOUT": "5",
        "MUSE_FIXTURE": str(fixture),
        "XDG_STATE_HOME": str(Path(tmp) / "state"),
        "XDG_CONFIG_HOME": str(Path(tmp) / "config"),
        "AGENT_METER_MUSE_PROBE": None,
    }
    if requests_log is not None:
        env["MUSE_REQUESTS_LOG"] = str(requests_log)
    return env


def _muse_memory_env(tmp: str, requests_log: Path | None = None) -> dict[str, str | None]:
    fake = Path(tmp) / "muse"
    _write_executable(fake, MUSE_MEMORY_FAKE)
    env: dict[str, str | None] = {
        "AGENT_METER_MUSE_BIN": str(fake),
        "AGENT_METER_MUSE_TIMEOUT": "5",
        "MUSE_USAGE_FIXTURE": str(FIXTURES / "muse-usage-observed.jsonl"),
        "XDG_STATE_HOME": str(Path(tmp) / "state"),
        "XDG_CONFIG_HOME": str(Path(tmp) / "config"),
        "AGENT_METER_MUSE_PROBE": None,
    }
    if requests_log is not None:
        env["MUSE_REQUESTS_LOG"] = str(requests_log)
    return env


def _muse_cache_path(tmp: str) -> Path:
    return Path(tmp) / "state" / "agent-meter" / "muse-usage.json"


def _seed_muse_cache(tmp: str, age_seconds: float) -> None:
    usage = json.loads(json.dumps(_observed_muse_usage()))
    usage.pop("tier", None)
    path = _muse_cache_path(tmp)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(usage) + "\n", encoding="utf-8")
    stamp = time.time() - age_seconds
    os.utime(path, (stamp, stamp))


def _write_muse_config(tmp: str, payload: dict) -> None:
    path = Path(tmp) / "config" / "agent-meter" / "muse.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload), encoding="utf-8")


def _probe_state_path(tmp: str) -> Path:
    return Path(tmp) / "state" / "agent-meter" / "muse-probe.json"


def _probe_log_path(tmp: str) -> Path:
    return Path(tmp) / "state" / "agent-meter" / "muse-probe.log"


def _turn_count(path: Path) -> int:
    if not path.exists():
        return 0
    return int(path.read_text(encoding="utf-8").strip() or "0")


def _probe_fake_env(tmp: str, mode: str, counter: Path, snapshot: Path | None = None) -> dict[str, str | None]:
    fake = Path(tmp) / "muse"
    _write_executable(fake, MUSE_PROBE_FAKE)
    env: dict[str, str | None] = {
        "AGENT_METER_MUSE_BIN": str(fake),
        "AGENT_METER_MUSE_TIMEOUT": "5",
        "AGENT_METER_MUSE_PROBE": "1",
        "AGENT_METER_MUSE_PROBE_TIMEOUT_SECONDS": None,
        "MUSE_PROBE_MODE": mode,
        "MUSE_TURN_COUNTER": str(counter),
        "MUSE_USAGE_FIXTURE": str(FIXTURES / "muse-usage-observed.jsonl"),
        "XDG_STATE_HOME": str(Path(tmp) / "state"),
        "XDG_CONFIG_HOME": str(Path(tmp) / "config"),
    }
    if snapshot is not None:
        env["MUSE_PROBE_SNAPSHOT"] = str(snapshot)
    return env


def _call_muse(now: adapter.dt.datetime | None = None) -> tuple[dict, str]:
    buffer = io.StringIO()
    with redirect_stderr(buffer):
        state = adapter.muse() if now is None else adapter.muse(now=now)
    return state, buffer.getvalue()


def _stderr_probe_lines(text: str) -> list[dict]:
    rows = []
    for line in text.splitlines():
        stripped = line.strip()
        if stripped:
            rows.append(json.loads(stripped))
    return rows


def _assert_private_file(path: Path) -> None:
    assert path.is_file()
    assert path.stat().st_mode & 0o777 == 0o600


def _load_probe_state(tmp: str) -> dict:
    path = _probe_state_path(tmp)
    _assert_private_file(path)
    return json.loads(path.read_text(encoding="utf-8"))


def _load_probe_log(tmp: str) -> list[dict]:
    path = _probe_log_path(tmp)
    _assert_private_file(path)
    text = path.read_text(encoding="utf-8")
    assert "REDACTED_TIER" not in text
    assert "token" not in text.lower()
    rows = [json.loads(line) for line in text.splitlines() if line.strip()]
    for row in rows:
        assert "tier" not in row["detail"].lower()
        assert "token" not in row["detail"].lower()
    return rows


def _assert_log_outcome(tmp: str, outcome: str, detail: str) -> dict:
    rows = _load_probe_log(tmp)
    row = rows[-1]
    assert row["outcome"] == outcome
    assert row["detail"] == detail
    assert isinstance(row["duration_ms"], int) and row["duration_ms"] >= 0
    assert isinstance(row["ts"], str) and row["ts"].endswith("Z")
    state = _load_probe_state(tmp)
    assert state["outcome"] == outcome
    assert state["detail"] == detail
    assert isinstance(state["last_attempt_at"], str) and state["last_attempt_at"].endswith("Z")
    return row


def _expected_throttle_detail(payload: dict, interval: float = 3600.0) -> str:
    last = adapter.dt.datetime.fromisoformat(payload["last_attempt_at"].replace("Z", "+00:00"))
    nxt = (last + adapter.dt.timedelta(seconds=interval)).astimezone(adapter.dt.UTC)
    return (
        f"Muse probe throttled: last attempt at {adapter._utc_stamp(last)} "
        f"outcome {payload['outcome']}; next probe allowed at {adapter._utc_stamp(nxt)}"
    )


def _assert_started_before_turn(snapshot: Path) -> dict:
    assert snapshot.is_file()
    raw = snapshot.read_text(encoding="utf-8")
    assert "REDACTED_TIER" not in raw
    started = json.loads(raw)
    assert started["outcome"] == "started"
    assert started["detail"] == adapter._MUSE_PROBE_DETAIL_STARTED
    assert started["last_attempt_at"].endswith("Z")
    return started


def _assert_uuid7(value: object) -> None:
    assert isinstance(value, str)
    parts = value.split("-")
    assert [len(part) for part in parts] == [8, 4, 4, 4, 12]
    int(value.replace("-", ""), 16)
    assert value[14] == "7"
    assert value[19] in "89ab"


def test_muse_empty_raises() -> None:
    try:
        adapter.normalize_muse({})
        assert False, "Expected ValueError"
    except ValueError:
        pass


def test_muse_observed_sample_windows() -> None:
    """Populated usage/read result from muse-usage-observed.jsonl."""
    usage = _observed_muse_usage()
    now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(minutes=1)
    state = adapter.normalize_muse({"usage": usage}, now=now)
    assert state["id"] == "muse"
    assert state["status"] == "fresh"
    assert state["detail"] == "Muse subscription usage"
    assert [window["id"] for window in state["windows"]] == ["session", "weekly"]
    session, weekly = state["windows"]
    assert session["label"] == "5h"
    assert session["remaining_percent"] == 100.0
    assert session["resets_at"] == "2026-10-14T05:50:15Z"
    assert weekly["label"] == "Weekly"
    assert weekly["remaining_percent"] == 52.0
    assert weekly["resets_at"] == "2026-10-19T00:00:00Z"
    encoded = json.dumps(state)
    assert "REDACTED_TIER" not in encoded
    assert "tier" not in state


def test_muse_observed_sample_stale_by_age() -> None:
    usage = _observed_muse_usage()
    now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(minutes=31)
    state = adapter.normalize_muse(usage, now=now)
    assert state["status"] == "stale"
    assert "minutes old" in state["detail"]
    session, weekly = state["windows"]
    assert session["remaining_percent"] == 100.0
    assert session["resets_at"] == "2026-10-14T05:50:15Z"
    assert weekly["remaining_percent"] == 52.0
    assert "resets_at" in weekly


def test_muse_observed_sample_reset_passed() -> None:
    usage = _observed_muse_usage()
    now = _ms(usage["window"]["resetsAtMs"]) + adapter.dt.timedelta(seconds=1)
    state = adapter.normalize_muse(usage, now=now)
    assert state["status"] == "stale"
    session = state["windows"][0]
    weekly = state["windows"][1]
    assert session["id"] == "session"
    assert session["remaining_percent"] == 100.0
    assert "resets_at" not in session
    assert weekly["id"] == "weekly"
    assert weekly["remaining_percent"] == 52.0
    assert weekly["resets_at"] == "2026-10-19T00:00:00Z"
    assert "reset" in state["detail"]


def test_muse_used_percent_above_100_clamps() -> None:
    usage = json.loads(json.dumps(_observed_muse_usage()))
    usage["window"]["usedPercent"] = 130
    now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(seconds=10)
    state = adapter.normalize_muse(usage, now=now)
    assert state["windows"][0]["remaining_percent"] == 0.0
    assert "resets_at" in state["windows"][0]


def test_muse_replay_observed_fixture() -> None:
    """Read-only usage/read is honored when this host process already has usage.

    The fixture replays an observation that already lives in this process. A
    later muse serve process does not inherit it.
    """
    with tempfile.TemporaryDirectory() as tmp:
        requests_log = Path(tmp) / "requests.json"
        env = _muse_env(tmp, FIXTURES / "muse-usage-observed.jsonl", requests_log)
        usage = _observed_muse_usage()
        now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(minutes=1)
        with patched_env(env):
            state = adapter.muse(now=now)
        requests = json.loads(requests_log.read_text(encoding="utf-8"))
        cache_path = Path(tmp) / "state" / "agent-meter" / "muse-usage.json"
        cache_text = cache_path.read_text(encoding="utf-8")
        cache = json.loads(cache_text)
    assert [item.get("method") for item in requests] == ["initialize", "initialized", "usage/read"]
    assert requests[0]["jsonrpc"] == "2.0"
    assert requests[0]["id"] == 1
    assert requests[0]["params"]["clientInfo"] == {"name": "agent_meter", "version": "0.2.0"}
    assert "id" not in requests[1]
    assert requests[2]["id"] == 2
    assert requests[2]["jsonrpc"] == "2.0"
    assert state["status"] == "fresh"
    assert [window["id"] for window in state["windows"]] == ["session", "weekly"]
    assert state["windows"][0]["label"] == "5h"
    assert state["windows"][0]["remaining_percent"] == 100.0
    assert state["windows"][1]["remaining_percent"] == 52.0
    assert "tier" not in cache
    assert "REDACTED_TIER" not in cache_text
    assert cache["observedAtMs"] == usage["observedAtMs"]


def test_muse_replay_usage_changed_notification() -> None:
    """usage/changed params are accepted when they arrive before a usage/read result."""
    usage = _observed_muse_usage()
    notification = _jsonl("muse-usage-observed.jsonl")[0]
    assert notification["method"] == "usage/changed"
    with tempfile.TemporaryDirectory() as tmp:
        fixture = Path(tmp) / "changed.jsonl"
        fixture.write_text(json.dumps(notification) + "\n", encoding="utf-8")
        now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(minutes=1)
        with patched_env(_muse_env(tmp, fixture)):
            state = adapter.muse(now=now)
    assert state["status"] == "fresh"
    assert [window["id"] for window in state["windows"]] == ["session", "weekly"]
    assert "REDACTED_TIER" not in json.dumps(state)


def test_muse_empty_read_is_unavailable_without_cache() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        with patched_env(_muse_env(tmp, FIXTURES / "muse-usage-read.jsonl")):
            state = adapter.muse()
        cache_exists = _muse_cache_path(tmp).exists()
    assert state["status"] == "unavailable"
    assert state["id"] == "muse"
    assert state["detail"] == MUSE_UNAVAILABLE_REASON
    assert state["detail"] == adapter._MUSE_NOT_REPORTED
    assert not cache_exists


def test_muse_empty_read_falls_back_to_cache() -> None:
    """An empty usage/read reuses agent-meter's cache, not Muse's disk.

    The second serve is a new process and returns {}. The numbers come from
    the cache file the first call wrote.
    """
    usage = _observed_muse_usage()
    now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(hours=2)
    with tempfile.TemporaryDirectory() as tmp:
        env = _muse_env(tmp, FIXTURES / "muse-usage-observed.jsonl")
        with patched_env(env):
            seeded = adapter.muse(now=_ms(usage["observedAtMs"]) + adapter.dt.timedelta(minutes=1))
            env["MUSE_FIXTURE"] = str(FIXTURES / "muse-usage-read.jsonl")
            with patched_env(env):
                state = adapter.muse(now=now)
    assert seeded["status"] == "fresh"
    assert state["status"] == "stale"
    assert state["windows"][0]["remaining_percent"] == 100.0
    assert state["windows"][1]["remaining_percent"] == 52.0
    assert "resets_at" in state["windows"][0]
    assert "minutes old" in state["detail"]


def test_muse_unavailable_when_missing() -> None:
    """muse() returns unavailable when the binary is not installed."""
    with patched_env({"AGENT_METER_MUSE_BIN": "/nonexistent/muse-for-test"}):
        state = adapter.muse()
    assert state["status"] == "unavailable"
    assert state["id"] == "muse"
    assert "not installed" in state["detail"]


def test_muse_serve_failure_is_unavailable() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        fake = Path(tmp) / "muse"
        _write_executable(fake, "#!/bin/sh\nexit 2\n")
        with patched_env({
            "AGENT_METER_MUSE_BIN": str(fake),
            "AGENT_METER_MUSE_TIMEOUT": "5",
            "XDG_STATE_HOME": str(Path(tmp) / "state"),
            "XDG_CONFIG_HOME": str(Path(tmp) / "config"),
            "AGENT_METER_MUSE_PROBE": None,
        }):
            state = adapter.muse()
    assert state["status"] == "unavailable"
    assert state["id"] == "muse"
    assert state["detail"]


def test_uuid7_helper() -> None:
    first = adapter.uuid7()
    second = adapter.uuid7()
    _assert_uuid7(first)
    _assert_uuid7(second)
    assert first != second
    unix_ms = int(first.replace("-", "")[:12], 16)
    assert abs(unix_ms - int(time.time() * 1000)) < 5000


def test_muse_probe_off_memory_host_is_unavailable() -> None:
    """A fresh host returns {} and the probe does not run or write a cache."""
    with tempfile.TemporaryDirectory() as tmp:
        requests_log = Path(tmp) / "requests.json"
        env = _muse_memory_env(tmp, requests_log)
        with patched_env(env):
            state = adapter.muse()
        requests = json.loads(requests_log.read_text(encoding="utf-8"))
        cache_exists = _muse_cache_path(tmp).exists()
    assert state["status"] == "unavailable"
    assert state["detail"] == MUSE_UNAVAILABLE_REASON
    assert [item.get("method") for item in requests] == ["initialize", "initialized", "usage/read"]
    assert not cache_exists


def test_muse_probe_on_sends_one_turn_and_caches() -> None:
    """The opt-in probe makes one minimal turn in the same muse serve process."""
    usage = _observed_muse_usage()
    now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(minutes=1)
    with tempfile.TemporaryDirectory() as tmp:
        requests_log = Path(tmp) / "requests.json"
        env = _muse_memory_env(tmp, requests_log)
        env["AGENT_METER_MUSE_PROBE"] = "1"
        with patched_env(env):
            state = adapter.muse(now=now)
        requests = json.loads(requests_log.read_text(encoding="utf-8"))
        cache_path = _muse_cache_path(tmp)
        cache = json.loads(cache_path.read_text(encoding="utf-8"))
        cache_text = cache_path.read_text(encoding="utf-8")
        # A new process does not keep the turn's in-memory usage.
        cache_path.unlink()
        env["AGENT_METER_MUSE_PROBE"] = None
        with patched_env(env):
            fresh = adapter.muse(now=now)
        assert not cache_path.exists()
    methods = [item.get("method") for item in requests]
    assert methods == [
        "initialize", "initialized", "usage/read",
        "session/start", "turn/start", "usage/read",
    ]
    session = requests[3]["params"]
    turn = requests[4]["params"]
    _assert_uuid7(session["commandId"])
    _assert_uuid7(turn["commandId"])
    assert session["commandId"] != turn["commandId"]
    assert os.path.isabs(session["workspaceRoot"])
    assert turn["sessionId"] == "sess-memory"
    assert turn["reasoningEffort"] == "minimal"
    assert turn["input"] == [{"type": "text", "text": "Reply with just: ok"}]
    assert state["status"] == "fresh"
    assert [window["id"] for window in state["windows"]] == ["session", "weekly"]
    assert state["windows"][0]["remaining_percent"] == 100.0
    assert state["windows"][1]["remaining_percent"] == 52.0
    assert "tier" not in cache
    assert "REDACTED_TIER" not in cache_text
    assert cache["observedAtMs"] == usage["observedAtMs"]
    assert fresh["status"] == "unavailable"
    assert fresh["detail"] == MUSE_UNAVAILABLE_REASON


def test_muse_probe_skips_turn_when_cache_is_fresh() -> None:
    usage = _observed_muse_usage()
    now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(minutes=1)
    with tempfile.TemporaryDirectory() as tmp:
        requests_log = Path(tmp) / "requests.json"
        _seed_muse_cache(tmp, age_seconds=60)
        env = _muse_memory_env(tmp, requests_log)
        env["AGENT_METER_MUSE_PROBE"] = "1"
        with patched_env(env):
            state = adapter.muse(now=now)
        requests = json.loads(requests_log.read_text(encoding="utf-8"))
    assert [item.get("method") for item in requests] == ["initialize", "initialized", "usage/read"]
    assert state["status"] == "fresh"
    assert state["windows"][0]["remaining_percent"] == 100.0
    assert state["windows"][1]["remaining_percent"] == 52.0


def test_muse_probe_interval_minimum_suppresses_a_short_config() -> None:
    """probe_interval_seconds below 900 is raised to 900, so a 500s cache waits."""
    usage = _observed_muse_usage()
    now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(minutes=1)
    with tempfile.TemporaryDirectory() as tmp:
        requests_log = Path(tmp) / "requests.json"
        _write_muse_config(tmp, {"probe": True, "probe_interval_seconds": 60})
        _seed_muse_cache(tmp, age_seconds=500)
        with patched_env(_muse_memory_env(tmp, requests_log)):
            state = adapter.muse(now=now)
        requests = json.loads(requests_log.read_text(encoding="utf-8"))
    assert "turn/start" not in [item.get("method") for item in requests]
    assert "session/start" not in [item.get("method") for item in requests]
    assert state["status"] == "fresh"


def test_muse_probe_runs_when_cache_is_older_than_interval() -> None:
    usage = _observed_muse_usage()
    now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(minutes=1)
    with tempfile.TemporaryDirectory() as tmp:
        requests_log = Path(tmp) / "requests.json"
        _write_muse_config(tmp, {"probe": True, "probe_interval_seconds": 900})
        _seed_muse_cache(tmp, age_seconds=1000)
        with patched_env(_muse_memory_env(tmp, requests_log)):
            state = adapter.muse(now=now)
        requests = json.loads(requests_log.read_text(encoding="utf-8"))
        cache_text = _muse_cache_path(tmp).read_text(encoding="utf-8")
    methods = [item.get("method") for item in requests]
    assert methods.count("turn/start") == 1
    assert methods.count("session/start") == 1
    assert state["status"] == "fresh"
    assert "tier" not in cache_text


def _probe_failure_then_refresh(mode: str, detail: str, *, cap_log: bool = False) -> None:
    """A failed probe records one turn; a second refresh inside the interval sends none."""
    with tempfile.TemporaryDirectory() as tmp:
        counter = Path(tmp) / "turns"
        snapshot = Path(tmp) / "started.json"
        env = _probe_fake_env(tmp, mode, counter, snapshot)
        if cap_log:
            log_path = _probe_log_path(tmp)
            log_path.parent.mkdir(parents=True, exist_ok=True)
            seeded = [
                json.dumps({
                    "ts": "2020-01-01T00:00:00Z",
                    "outcome": "failure",
                    "duration_ms": 1,
                    "detail": f"old-{index}",
                }, separators=(",", ":"))
                for index in range(200)
            ]
            log_path.write_text("\n".join(seeded) + "\n", encoding="utf-8")
        with patched_env(env):
            first, first_err = _call_muse()
            assert _turn_count(counter) == 1
            second, second_err = _call_muse()
        assert _turn_count(counter) == 1
        started = _assert_started_before_turn(snapshot)
        final = _load_probe_state(tmp)
        assert final["last_attempt_at"] == started["last_attempt_at"]
        assert final["outcome"] == "failure"
        assert final["detail"] == detail
        row = _assert_log_outcome(tmp, "failure", detail)
        stderr_rows = _stderr_probe_lines(first_err)
        assert len(stderr_rows) == 1
        assert stderr_rows[0]["outcome"] == "failure"
        assert stderr_rows[0]["detail"] == detail
        assert stderr_rows[0]["duration_ms"] == row["duration_ms"]
        assert _stderr_probe_lines(second_err) == []
        assert first["status"] == "unavailable"
        assert second["status"] == "unavailable"
        assert second["detail"] == _expected_throttle_detail(final)
        assert "tier" not in second["detail"].lower()
        if cap_log:
            rows = _load_probe_log(tmp)
            assert len(rows) == 200
            assert rows[0]["detail"] == "old-1"
            assert rows[-1]["detail"] == detail
            assert all(item["detail"] != "old-0" for item in rows)


def test_muse_probe_rpc_error_is_not_retried_within_interval() -> None:
    """(a) JSON-RPC error on the probe turn. The next refresh sends no turn."""
    _probe_failure_then_refresh("error", "Muse turn/start failed", cap_log=True)


def test_muse_probe_exit_is_not_retried_within_interval() -> None:
    """(a) The fake exits nonzero after the turn. The next refresh sends no turn."""
    _probe_failure_then_refresh("exit", "muse serve ended before the probe turn completed")


def test_muse_probe_timeout_is_not_retried_within_interval() -> None:
    """(b) A hanging fake times out; the next refresh sends no turn."""
    with tempfile.TemporaryDirectory() as tmp:
        counter = Path(tmp) / "turns"
        snapshot = Path(tmp) / "started.json"
        env = _probe_fake_env(tmp, "hang", counter, snapshot)
        env["AGENT_METER_MUSE_PROBE_TIMEOUT_SECONDS"] = "2"
        with patched_env(env):
            first, first_err = _call_muse()
            assert _turn_count(counter) == 1
            second, second_err = _call_muse()
        assert _turn_count(counter) == 1
        started = _assert_started_before_turn(snapshot)
        final = _load_probe_state(tmp)
        assert final["last_attempt_at"] == started["last_attempt_at"]
        assert final["outcome"] == "timeout"
        detail = adapter._MUSE_PROBE_DETAIL_TIMEOUT
        row = _assert_log_outcome(tmp, "timeout", detail)
        assert row["duration_ms"] < 15000
        stderr_rows = _stderr_probe_lines(first_err)
        assert len(stderr_rows) == 1
        assert stderr_rows[0]["outcome"] == "timeout"
        assert stderr_rows[0]["detail"] == detail
        assert _stderr_probe_lines(second_err) == []
        assert first["status"] == "unavailable"
        assert second["status"] == "unavailable"
        assert second["detail"] == _expected_throttle_detail(final)


def test_muse_probe_empty_is_not_retried_within_interval() -> None:
    """(c) A turn whose usage/read is {} does not run again inside the interval."""
    with tempfile.TemporaryDirectory() as tmp:
        counter = Path(tmp) / "turns"
        snapshot = Path(tmp) / "started.json"
        env = _probe_fake_env(tmp, "empty", counter, snapshot)
        with patched_env(env):
            first, first_err = _call_muse()
            assert _turn_count(counter) == 1
            second, second_err = _call_muse()
        assert _turn_count(counter) == 1
        started = _assert_started_before_turn(snapshot)
        final = _load_probe_state(tmp)
        assert final["last_attempt_at"] == started["last_attempt_at"]
        detail = adapter._MUSE_PROBE_DETAIL_EMPTY
        assert final["outcome"] == "empty"
        assert final["detail"] == detail
        _assert_log_outcome(tmp, "empty", detail)
        stderr_rows = _stderr_probe_lines(first_err)
        assert len(stderr_rows) == 1
        assert stderr_rows[0]["outcome"] == "empty"
        assert _stderr_probe_lines(second_err) == []
        assert first["status"] == "unavailable"
        assert first["detail"] == MUSE_UNAVAILABLE_REASON
        assert second["status"] == "unavailable"
        assert second["detail"] == _expected_throttle_detail(final)
        assert not _muse_cache_path(tmp).exists()


def test_muse_probe_bad_usage_cache_and_recent_attempt_does_not_reprobe() -> None:
    """(d) Corrupt or unreadable muse-usage.json plus a recent attempt sends no turn."""
    for bad in ("corrupt", "unreadable"):
        with tempfile.TemporaryDirectory() as tmp:
            counter = Path(tmp) / "turns"
            snapshot = Path(tmp) / "started.json"
            env = _probe_fake_env(tmp, "error", counter, snapshot)
            with patched_env(env):
                _call_muse()
                assert _turn_count(counter) == 1
                recorded = _load_probe_state(tmp)
                cache = _muse_cache_path(tmp)
                cache.parent.mkdir(parents=True, exist_ok=True)
                if bad == "corrupt":
                    cache.write_text("{", encoding="utf-8")
                else:
                    cache.mkdir()
                second, second_err = _call_muse()
            assert _turn_count(counter) == 1, bad
            final = _load_probe_state(tmp)
            assert final == recorded
            assert final["outcome"] == "failure"
            _assert_log_outcome(tmp, "failure", "Muse turn/start failed")
            assert len(_load_probe_log(tmp)) == 1
            assert _stderr_probe_lines(second_err) == []
            assert second["status"] == "unavailable"
            assert second["detail"] == _expected_throttle_detail(final)


def test_muse_probe_corrupt_state_fails_closed() -> None:
    """(e) A corrupt muse-probe.json does not probe, and the file is rewritten."""
    samples = ["{not-json", '{"outcome": "success"}', None]
    for sample in samples:
        with tempfile.TemporaryDirectory() as tmp:
            counter = Path(tmp) / "turns"
            env = _probe_fake_env(tmp, "ok", counter)
            path = _probe_state_path(tmp)
            path.parent.mkdir(parents=True, exist_ok=True)
            if sample is None:
                path.mkdir()
            else:
                path.write_text(sample, encoding="utf-8")
            with patched_env(env):
                state, err = _call_muse()
                again, again_err = _call_muse()
            assert _turn_count(counter) == 0
            detail = adapter._MUSE_PROBE_DETAIL_UNREADABLE
            final = _load_probe_state(tmp)
            assert final["outcome"] == "failure"
            assert final["detail"] == detail
            last = adapter.dt.datetime.fromisoformat(final["last_attempt_at"].replace("Z", "+00:00"))
            assert abs((adapter.dt.datetime.now(adapter.dt.UTC) - last).total_seconds()) < 30
            rows = _load_probe_log(tmp)
            assert len(rows) == 1
            assert rows[0]["outcome"] == "failure"
            assert rows[0]["detail"] == detail
            assert rows[0]["duration_ms"] == 0
            stderr_rows = _stderr_probe_lines(err)
            assert len(stderr_rows) == 1
            assert stderr_rows[0]["outcome"] == "failure"
            assert stderr_rows[0]["detail"] == detail
            assert _stderr_probe_lines(again_err) == []
            assert state["status"] == "unavailable"
            assert state["detail"] == _expected_throttle_detail(final)
            assert again["detail"] == state["detail"]
            assert _turn_count(counter) == 0


def test_muse_probe_after_interval_probes_once() -> None:
    """(f) An attempt older than the interval probes once, even with a fresh usage cache."""
    usage = _observed_muse_usage()
    now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(minutes=1)
    with tempfile.TemporaryDirectory() as tmp:
        counter = Path(tmp) / "turns"
        snapshot = Path(tmp) / "started.json"
        env = _probe_fake_env(tmp, "ok", counter, snapshot)
        _seed_muse_cache(tmp, age_seconds=30)
        previous = adapter.dt.datetime.now(adapter.dt.UTC) - adapter.dt.timedelta(hours=3)
        path = _probe_state_path(tmp)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({
            "last_attempt_at": adapter._utc_stamp(previous),
            "outcome": "failure",
            "detail": "previous failure",
        }) + "\n", encoding="utf-8")
        with patched_env(env):
            state, err = _call_muse(now)
            assert _turn_count(counter) == 1
            second, second_err = _call_muse(now)
        assert _turn_count(counter) == 1
        started = _assert_started_before_turn(snapshot)
        final = _load_probe_state(tmp)
        assert final["last_attempt_at"] == started["last_attempt_at"]
        detail = adapter._MUSE_PROBE_DETAIL_SUCCESS
        assert final["outcome"] == "success"
        assert final["detail"] == detail
        rows = _load_probe_log(tmp)
        assert len(rows) == 1
        assert rows[0]["outcome"] == "success"
        assert rows[0]["detail"] == detail
        assert "REDACTED_TIER" not in _probe_log_path(tmp).read_text(encoding="utf-8")
        assert "REDACTED_TIER" not in path.read_text(encoding="utf-8")
        stderr_rows = _stderr_probe_lines(err)
        assert len(stderr_rows) == 1
        assert stderr_rows[0]["outcome"] == "success"
        assert _stderr_probe_lines(second_err) == []
        assert state["status"] == "fresh"
        assert [window["id"] for window in state["windows"]] == ["session", "weekly"]
        assert second["status"] == "fresh"
        cache_text = _muse_cache_path(tmp).read_text(encoding="utf-8")
        assert "tier" not in json.loads(cache_text)
        assert "REDACTED_TIER" not in cache_text


def test_grok_omp_data_is_always_fresh() -> None:
    """OMP OAuth data without timestamp is live; the adapter should return fresh, not stale."""
    state = adapter.normalize_omp_grok_usage({"reports": [{"provider": "xai-oauth", "limits": [{
        "id": "xai-oauth:credits:1w", "status": "ok",
        "window": {"resetsAt": 1788572730851},
        "amount": {"remainingFraction": 0.75},
    }]}]})
    assert state["status"] == "fresh"


def test_grok_omp_live_fetch_ignores_payload_age() -> None:
    """OMP usage is a live report, stamped with the fetch time, not a payload clock."""
    now = adapter.dt.datetime(2026, 9, 15, 12, 0, 0, tzinfo=adapter.dt.UTC)
    old_ts = (now - adapter.dt.timedelta(minutes=45)).isoformat()
    state = adapter.normalize_omp_grok_usage({
        "reports": [{
            "provider": "xai-oauth",
            "updatedAt": old_ts,
            "limits": [{
                "id": "xai-oauth:credits:1w",
                "status": "ok",
                "window": {"resetsAt": 1788572730851},
                "amount": {"remainingFraction": 0.60},
            }],
        }],
    }, now=now)
    assert state["status"] == "fresh"
    assert "OMP OAuth" in state["detail"]


def test_grok_omp_marks_recent_report_fresh() -> None:
    """OMP OAuth data younger than 30 minutes must be fresh."""
    now = adapter.dt.datetime(2026, 9, 15, 12, 0, 0, tzinfo=adapter.dt.UTC)
    # 10 minutes old
    recent_ts = (now - adapter.dt.timedelta(minutes=10)).isoformat()
    state = adapter.normalize_omp_grok_usage({
        "reports": [{
            "provider": "xai-oauth",
            "updatedAt": recent_ts,
            "limits": [{
                "id": "xai-oauth:credits:1w",
                "status": "ok",
                "window": {"resetsAt": 1788572730851},
                "amount": {"remainingFraction": 0.60},
            }],
        }],
    }, now=now)
    assert state["status"] == "fresh"


def test_grok_log_marks_recent_snapshot_fresh() -> None:
    """Grok Build log under 30 minutes old must be fresh."""
    now = adapter.dt.datetime(2026, 8, 30, 20, 15, tzinfo=adapter.dt.UTC)
    state = adapter.normalize_grok([{
        "ts": "2026-08-30T20:00:00Z",
        "msg": "billing: fetched credits config",
        "ctx": {"config": {
            "creditUsagePercent": 55,
            "billingPeriodEnd": "2026-09-05T00:00:00Z",
        }},
    }], now=now)
    assert state["status"] == "fresh"


def _grok_sample_records() -> list:
    return _jsonl("grok-unified-sample.jsonl")


def test_grok_sample_fresh_within_period() -> None:
    """Newest billing snapshot, shortly after its ts and still inside the period."""
    now = adapter.dt.datetime(2026, 10, 9, 22, 50, tzinfo=adapter.dt.UTC)
    state = adapter.normalize_grok(_grok_sample_records(), now=now)
    assert state["status"] == "fresh"
    assert state["windows"][0]["id"] == "weekly"
    assert state["windows"][0]["remaining_percent"] == 0.0
    assert state["windows"][0]["resets_at"] == "2026-10-10T01:45:30.851142Z"


def test_grok_sample_stale_within_period() -> None:
    """More than 30 minutes after the snapshot, billing period still open."""
    now = adapter.dt.datetime(2026, 10, 9, 23, 30, tzinfo=adapter.dt.UTC)
    state = adapter.normalize_grok(_grok_sample_records(), now=now)
    assert state["status"] == "stale"
    assert "minutes old" in state["detail"]
    assert state["windows"][0]["remaining_percent"] == 0.0
    assert state["windows"][0]["resets_at"] == "2026-10-10T01:45:30.851142Z"


def test_grok_sample_after_period_end() -> None:
    """Ended period stays visible, stale, with no reset instant in the past."""
    now = adapter.dt.datetime(2026, 10, 10, 2, 0, tzinfo=adapter.dt.UTC)
    state = adapter.normalize_grok(_grok_sample_records(), now=now)
    assert state["status"] == "stale"
    assert state["windows"][0]["remaining_percent"] == 0.0
    assert "resets_at" not in state["windows"][0]
    assert "billing period that ended" in state["detail"]
    assert "start Grok Build to refresh" in state["detail"]


def test_grok_sample_without_billing_records_is_unavailable() -> None:
    records = [row for row in _grok_sample_records() if row.get("msg") != "billing: fetched credits config"]
    assert records, "sample should include non-billing log lines"
    try:
        adapter.normalize_grok(records, now=adapter.dt.datetime(2026, 10, 12, tzinfo=adapter.dt.UTC))
        raise AssertionError("expected ValueError")
    except ValueError:
        pass
    with tempfile.TemporaryDirectory() as tmp:
        log_path = Path(tmp) / "unified.jsonl"
        log_path.write_text("".join(json.dumps(row) + "\n" for row in records), encoding="utf-8")
        with patched_env({
            "AGENT_METER_OMP_BIN": "/nonexistent/omp-for-test",
            "AGENT_METER_GROK_LOG": str(log_path),
        }):
            state = adapter.grok()
    assert state["status"] == "unavailable"
    assert state["id"] == "grok"
    assert "Grok usage unavailable" in state["detail"]


def test_grok_log_used_when_omp_missing() -> None:
    """OMP missing must not block the Grok Build log path."""
    now = adapter.dt.datetime(2026, 10, 9, 22, 50, tzinfo=adapter.dt.UTC)
    with patched_env({
        "AGENT_METER_OMP_BIN": "/nonexistent/omp-for-test",
        "AGENT_METER_GROK_LOG": str(FIXTURES / "grok-unified-sample.jsonl"),
    }):
        state = adapter.grok(now=now)
    assert state["status"] == "fresh"
    assert state["windows"][0]["remaining_percent"] == 0.0
    assert state["windows"][0]["resets_at"] == "2026-10-10T01:45:30.851142Z"
    assert "OMP" not in state["detail"]


def test_copilot_quota_sample_has_no_trustworthy_reset() -> None:
    """Copilot SDK resetDate matches the fetch time, so it is not a reset instant.

    The captured account.getQuota sample reports every resetDate within a second
    of fetched_at. Those windows omit resets_at and use reset_label "Reset unknown".
    """
    payload = json.loads((FIXTURES / "copilot-quota.json").read_text(encoding="utf-8"))
    now = adapter.dt.datetime.fromisoformat(payload["fetched_at"].replace("Z", "+00:00"))
    state = adapter.normalize_copilot_quota(payload, now=now)
    assert state["windows"], "sample should produce quota windows"
    for window in state["windows"]:
        assert "resets_at" not in window
        assert window["reset_label"] == "Reset unknown"
    premium = state["windows"][0]
    assert premium["id"] == "premium_interactions"
    assert premium["remaining_percent"] == 0.0
    assert state["status"] == "fresh"
    assert "200 of 200 used" in state["detail"]


def test_resets_at_iso8601_all_providers() -> None:
    """Provider windows expose resets_at as ISO 8601 when a trustworthy reset exists.

    Copilot is excluded. The Copilot SDK account.getQuota returns resetDate equal
    to the fetch time, so no trustworthy reset instant exists. See
    test_copilot_quota_sample_has_no_trustworthy_reset.
    """
    now = adapter.dt.datetime(2026, 9, 1, 12, 0, 0, tzinfo=adapter.dt.UTC)

    # 1. Codex
    codex_state = adapter.normalize_codex({
        "rateLimits": {
            "primary": {"usedPercent": 20, "windowDurationMins": 300, "resetsAt": 1_788_000_000},
            "secondary": {"usedPercent": 30, "windowDurationMins": 10_080, "resetsAt": "2026-09-20T00:00:00Z"},
        }
    })
    for w in codex_state["windows"]:
        assert "resets_at" in w
        adapter.dt.datetime.fromisoformat(w["resets_at"].replace("Z", "+00:00"))

    # 2. Antigravity
    agy_state = adapter.normalize_agy({
        "quota_summary": {
            "groups": [{
                "display_name": "Models",
                "buckets": [
                    {"window": "5h", "remaining_pct": 75.0, "reset_time": "2026-09-16T20:00:00Z"},
                    {"window": "Weekly", "remaining_pct": 90.0, "reset_time": 1_788_000_000},
                ],
            }]
        }
    })
    for w in agy_state["windows"]:
        assert "resets_at" in w
        adapter.dt.datetime.fromisoformat(w["resets_at"].replace("Z", "+00:00"))

    # 3. Claude
    claude_state = adapter.normalize_claude({
        "rate_limits": {
            "five_hour": {"used_percentage": 25, "resets_at": 1788000000},
            "seven_day": {"used_percentage": 40, "resets_at": "2026-09-20T00:00:00Z"},
        }
    })
    for w in claude_state["windows"]:
        assert "resets_at" in w
        adapter.dt.datetime.fromisoformat(w["resets_at"].replace("Z", "+00:00"))

    # 4. Grok Build log
    grok_log_state = adapter.normalize_grok([{
        "ts": "2026-08-30T20:00:00Z",
        "msg": "billing: fetched credits config",
        "ctx": {"config": {
            "creditUsagePercent": 50,
            "billingPeriodEnd": "2026-09-05T00:00:00Z",
        }},
    }], now=now)
    for w in grok_log_state["windows"]:
        assert "resets_at" in w
        adapter.dt.datetime.fromisoformat(w["resets_at"].replace("Z", "+00:00"))

    # 5. Grok OMP
    grok_omp_state = adapter.normalize_omp_grok_usage({
        "reports": [{
            "provider": "xai-oauth",
            "limits": [{
                "id": "xai-oauth:credits:1w",
                "status": "ok",
                "window": {"resetsAt": 1788572730851},
                "amount": {"remainingFraction": 0.8},
            }],
        }]
    })
    for w in grok_omp_state["windows"]:
        assert "resets_at" in w
        adapter.dt.datetime.fromisoformat(w["resets_at"].replace("Z", "+00:00"))

    # 6. Copilot is intentionally not asserted here. account.getQuota's resetDate
    # equals the fetch time, so the windows have no ISO 8601 resets_at.

    # 7. Muse, from the captured SubscriptionUsage sample, before either reset.
    usage = _observed_muse_usage()
    muse_now = _ms(usage["observedAtMs"]) + adapter.dt.timedelta(minutes=1)
    muse_state = adapter.normalize_muse(usage, now=muse_now)
    for w in muse_state["windows"]:
        assert "resets_at" in w
        adapter.dt.datetime.fromisoformat(w["resets_at"].replace("Z", "+00:00"))


def test_json_schema_output() -> None:
    """agent-meter --json conforms to schema {schema_version:1, providers:[...]}."""
    import subprocess
    root = Path(__file__).resolve().parents[1]
    binary = root / "target" / "debug" / "agent-meter"
    subprocess.run(["cargo", "build", "--bin", "agent-meter"], check=True, cwd=root, capture_output=True)

    with tempfile.TemporaryDirectory() as tmp:
        state_file = Path(tmp) / "state.json"
        sample_state = {
            "version": 1,
            "generated_at": "2026-09-16T20:45:00Z",
            "providers": [
                {
                    "id": "codex",
                    "label": "Codex",
                    "icon": "codex",
                    "windows": [
                        {"id": "five_hour", "label": "5 hours", "remaining_percent": 72.0, "resets_at": "2026-09-16T22:00:00Z"},
                        {"id": "weekly", "label": "Weekly", "remaining_percent": 51.0, "resets_at": "2026-09-20T14:00:00Z"},
                    ],
                    "status": "fresh",
                },
                {
                    "id": "grok",
                    "label": "Grok",
                    "icon": "grok",
                    "windows": [
                        {"id": "weekly", "label": "Weekly", "remaining_percent": 45.0, "resets_at": "2026-09-20T01:45:30Z"},
                    ],
                    "status": "stale",
                },
                {
                    "id": "muse",
                    "label": "Muse",
                    "icon": "generic",
                    "windows": [
                        {"id": "availability", "label": "Availability", "remaining_percent": 0.0},
                    ],
                    "status": "unavailable",
                },
            ],
        }
        state_file.write_text(json.dumps(sample_state))
        proc = subprocess.run(
            [str(binary), "--json", "--state", str(state_file)],
            capture_output=True, text=True, check=True,
        )
        data = json.loads(proc.stdout)
        assert data["schema_version"] == 1
        assert isinstance(data["providers"], list)
        assert len(data["providers"]) == 3

        codex = data["providers"][0]
        assert codex["provider"] == "codex"
        assert codex["available"] is True
        assert codex["stale"] is False
        assert isinstance(codex["updated_at"], str)
        assert len(codex["windows"]) == 2
        assert codex["windows"][0]["name"] == "five_hour"
        assert codex["windows"][0]["remaining_percent"] == 72.0
        assert "2026-09-16" in codex["windows"][0]["resets_at"]

        grok = data["providers"][1]
        assert grok["provider"] == "grok"
        assert grok["available"] is True
        assert grok["stale"] is True
        assert grok["windows"][0]["name"] == "weekly"

        muse = data["providers"][2]
        assert muse["provider"] == "muse"
        assert muse["available"] is False
        assert muse["stale"] is False


if __name__ == "__main__":
    test_reset_calendar_and_timezone()
    test_copilot_reset_validity()
    test_codex()
    test_codex_rateLimitsByLimitId()
    test_codex_hung_app_server_times_out()
    test_codex_hung_after_init_times_out()
    test_codex_app_server_success()
    test_agy()
    test_agy_disabled_bucket_skipped()
    test_agy_provider_usage_command()
    test_claude_normalize()
    test_claude_normalize_only_one_window()
    test_claude_normalize_missing_rate_limits()
    test_claude_absent()
    test_claude_reads_normalized_state()
    test_claude_reads_raw_statusline_json()
    test_claude_marks_expired_state_stale()
    test_copilot_usage_items()
    test_copilot_with_allowance()
    test_copilot_requires_explicit_allowance()
    test_copilot_no_credits_raises()
    test_copilot_allowance_clamps_to_zero()
    test_copilot_total_premium_requests()
    test_copilot_official_undated_response()
    test_copilot_sdk_quota()
    test_copilot_env_allowance()
    test_grok_billing_snapshot()
    test_omp_grok_usage_uses_shared_limit()
    test_omp_grok_shared_statuses()
    test_grok_ended_period_is_stale_without_past_reset()
    test_grok_marks_old_snapshot_stale()
    test_grok_never_infers_quota_from_auth_file()
    test_grok_omp_data_is_always_fresh()
    test_grok_omp_live_fetch_ignores_payload_age()
    test_grok_omp_marks_recent_report_fresh()
    test_grok_log_marks_recent_snapshot_fresh()
    test_grok_sample_fresh_within_period()
    test_grok_sample_stale_within_period()
    test_grok_sample_after_period_end()
    test_grok_sample_without_billing_records_is_unavailable()
    test_grok_log_used_when_omp_missing()
    test_copilot_quota_sample_has_no_trustworthy_reset()
    test_muse_empty_raises()
    test_muse_observed_sample_windows()
    test_muse_observed_sample_stale_by_age()
    test_muse_observed_sample_reset_passed()
    test_muse_used_percent_above_100_clamps()
    test_muse_replay_observed_fixture()
    test_muse_replay_usage_changed_notification()
    test_muse_empty_read_is_unavailable_without_cache()
    test_muse_empty_read_falls_back_to_cache()
    test_muse_unavailable_when_missing()
    test_muse_serve_failure_is_unavailable()
    test_uuid7_helper()
    test_muse_probe_off_memory_host_is_unavailable()
    test_muse_probe_on_sends_one_turn_and_caches()
    test_muse_probe_skips_turn_when_cache_is_fresh()
    test_muse_probe_interval_minimum_suppresses_a_short_config()
    test_muse_probe_runs_when_cache_is_older_than_interval()
    test_muse_probe_rpc_error_is_not_retried_within_interval()
    test_muse_probe_exit_is_not_retried_within_interval()
    test_muse_probe_timeout_is_not_retried_within_interval()
    test_muse_probe_empty_is_not_retried_within_interval()
    test_muse_probe_bad_usage_cache_and_recent_attempt_does_not_reprobe()
    test_muse_probe_corrupt_state_fails_closed()
    test_muse_probe_after_interval_probes_once()
    test_resets_at_iso8601_all_providers()
    test_json_schema_output()
    print("Adapter normalization checks passed")
