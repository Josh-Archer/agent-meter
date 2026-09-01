#!/usr/bin/env python3
import json
import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "adapters"))
import agent_meter_adapter as adapter


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


if __name__ == "__main__":
    test_codex()
    test_codex_rateLimitsByLimitId()
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
    test_grok_ignores_expired_snapshot()
    test_grok_marks_old_snapshot_stale()
    test_grok_never_infers_quota_from_auth_file()
    print("Adapter normalization checks passed")
