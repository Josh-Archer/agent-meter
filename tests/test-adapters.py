#!/usr/bin/env python3
import json
import os
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "adapters"))
import agent_meter_adapter as adapter


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


def test_grok_ignores_expired_snapshot() -> None:
    now = adapter.dt.datetime(2026, 8, 30, tzinfo=adapter.dt.UTC)
    try:
        adapter.normalize_grok([{"ts": "2026-08-20T00:00:00Z", "msg": "billing: fetched credits config", "ctx": {"config": {
            "creditUsagePercent": 47,
            "billingPeriodEnd": "2026-08-25T00:00:00Z",
        }}}], now=now)
        assert False, "Expected ValueError"
    except ValueError:
        pass


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
# normalize_muse
# ---------------------------------------------------------------------------

def test_muse_usage_with_windows() -> None:
    state = adapter.normalize_muse({"usage": {"windows": [
        {"name": "five_hour", "remaining_percent": 65.0, "resets_at": "2026-09-16T21:52:00Z"},
        {"name": "weekly", "remaining_percent": 80.0, "resets_at": "2026-09-20T00:00:00Z"},
    ]}})
    assert state["id"] == "muse"
    assert state["status"] == "fresh"
    assert len(state["windows"]) == 2
    assert state["windows"][0]["remaining_percent"] == 65.0
    assert state["windows"][0]["resets_at"] == "2026-09-16T21:52:00Z"
    assert state["windows"][1]["remaining_percent"] == 80.0


def test_muse_usage_with_buckets() -> None:
    state = adapter.normalize_muse({"quota": {"buckets": [
        {"name": "hourly", "remaining_fraction": 0.42, "reset_time": 1788000000},
    ]}})
    assert state["windows"][0]["remaining_percent"] == 42.0


def test_muse_flat_remaining() -> None:
    state = adapter.normalize_muse({"usage": {"remaining_percent": 55.0, "resets_at": "2026-09-20T00:00:00Z"}})
    assert len(state["windows"]) == 1
    assert state["windows"][0]["remaining_percent"] == 55.0


def test_muse_used_percentage() -> None:
    state = adapter.normalize_muse({"usage": {"windows": [
        {"name": "daily", "used_percentage": 30},
    ]}})
    assert state["windows"][0]["remaining_percent"] == 70.0


def test_muse_direct_windows() -> None:
    state = adapter.normalize_muse({"windows": [
        {"name": "weekly", "remaining_percent": 88.5},
    ]})
    assert state["windows"][0]["remaining_percent"] == 88.5


def test_muse_empty_raises() -> None:
    try:
        adapter.normalize_muse({})
        assert False, "Expected ValueError"
    except ValueError:
        pass


def test_muse_unavailable_when_missing() -> None:
    """muse() gracefully returns unavailable when the binary is not found."""
    old = os.environ.get("AGENT_METER_MUSE_BIN")
    os.environ["AGENT_METER_MUSE_BIN"] = "/nonexistent/muse-for-test"
    try:
        state = adapter.muse()
    finally:
        if old is None:
            os.environ.pop("AGENT_METER_MUSE_BIN", None)
        else:
            os.environ["AGENT_METER_MUSE_BIN"] = old
    assert state["status"] == "unavailable"
    assert state["id"] == "muse"


def test_grok_omp_data_is_always_fresh() -> None:
    """OMP OAuth data without timestamp is live; the adapter should return fresh, not stale."""
    state = adapter.normalize_omp_grok_usage({"reports": [{"provider": "xai-oauth", "limits": [{
        "id": "xai-oauth:credits:1w", "status": "ok",
        "window": {"resetsAt": 1788572730851},
        "amount": {"remainingFraction": 0.75},
    }]}]})
    assert state["status"] == "fresh"


def test_grok_omp_marks_old_report_stale() -> None:
    """OMP OAuth data older than 30 minutes must be marked stale: true."""
    now = adapter.dt.datetime(2026, 9, 15, 12, 0, 0, tzinfo=adapter.dt.UTC)
    # 45 minutes old
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
    assert state["status"] == "stale"
    assert "old" in state["detail"]


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


def test_muse_rate_limits_key() -> None:
    """Muse adapter accepts rate_limits key."""
    state = adapter.normalize_muse({"rate_limits": {
        "windows": [
            {"name": "five_hour", "remaining_percent": 85.0, "resets_at": "2026-09-16T21:52:00Z"},
        ]
    }})
    assert state["id"] == "muse"
    assert state["windows"][0]["remaining_percent"] == 85.0
    assert state["windows"][0]["resets_at"] == "2026-09-16T21:52:00Z"


def test_resets_at_iso8601_all_providers() -> None:
    """Every provider window must provide resets_at in ISO8601 when reset time is available."""
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

    # 6. Copilot
    copilot_state = adapter.normalize_copilot_quota({
        "quotaSnapshots": {
            "premium_interactions": {
                "remainingPercentage": 80,
                "resetDate": "2026-10-01T00:00:00Z",
            }
        }
    }, now=now)
    for w in copilot_state["windows"]:
        assert "resets_at" in w
        adapter.dt.datetime.fromisoformat(w["resets_at"].replace("Z", "+00:00"))

    # 7. Muse
    muse_state = adapter.normalize_muse({
        "usage": {
            "windows": [
                {"name": "five_hour", "remaining_percent": 60.0, "resets_at": "2026-09-16T21:52:00Z"},
                {"name": "weekly", "remaining_percent": 85.0, "reset_time": 1788000000},
            ]
        }
    })
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
        assert len(data["providers"]) == 2

        codex = data["providers"][0]
        assert codex["provider"] == "codex"
        assert codex["stale"] is False
        assert isinstance(codex["updated_at"], str)
        assert len(codex["windows"]) == 2
        assert codex["windows"][0]["name"] == "five_hour"
        assert codex["windows"][0]["remaining_percent"] == 72.0
        assert "2026-09-16" in codex["windows"][0]["resets_at"]

        grok = data["providers"][1]
        assert grok["provider"] == "grok"
        assert grok["stale"] is True
        assert grok["windows"][0]["name"] == "weekly"


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
    test_grok_ignores_expired_snapshot()
    test_grok_marks_old_snapshot_stale()
    test_grok_never_infers_quota_from_auth_file()
    test_grok_omp_data_is_always_fresh()
    test_grok_omp_marks_old_report_stale()
    test_grok_omp_marks_recent_report_fresh()
    test_grok_log_marks_recent_snapshot_fresh()
    test_muse_usage_with_windows()
    test_muse_usage_with_buckets()
    test_muse_flat_remaining()
    test_muse_used_percentage()
    test_muse_direct_windows()
    test_muse_empty_raises()
    test_muse_unavailable_when_missing()
    test_muse_rate_limits_key()
    test_resets_at_iso8601_all_providers()
    test_json_schema_output()
    print("Adapter normalization checks passed")
