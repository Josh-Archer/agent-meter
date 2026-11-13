//! Inbox drain through the real `agent-meterd` binary, then the usage CLI.
//! The listener is not started.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use agent_meter::usage;

fn debug_bin(name: &str) -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
    target.join("debug").join(name)
}

#[test]
fn daemon_drains_inbox_and_usage_cli_prints_the_event() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let state_home = dir.path().join("state");
    let runtime = dir.path().join("runtime");
    let config_home = dir.path().join("config");
    fs::create_dir_all(config_home.join("agent-meter")).unwrap();
    fs::create_dir_all(&runtime).unwrap();
    let sources = config_home.join("agent-meter").join("sources.json");
    fs::write(&sources, r#"{"refresh_seconds":60,"sources":[]}"#).unwrap();

    let root = state_home.join("agent-meter");
    let inbox = root.join("inbox");
    fs::create_dir_all(&inbox).unwrap();

    let host = "homelabdesktop";
    let harness = "claude";
    let session = "round-trip";
    let seq = 3u64;
    let observed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let event = serde_json::json!({
        "schema": "agent-meter.usage-event/v1",
        "idempotency_key": usage::canonical_idempotency_key(host, harness, session, seq),
        "kind": "session_snapshot",
        "host": host,
        "harness": harness,
        "harness_version": "2.1.321",
        "session_id": session,
        "seq": seq,
        "observed_at": observed_at,
        "started_at": null,
        "ended_at": null,
        "duration_ms": 45000,
        "models": [{
            "model": "claude-opus-5-5",
            "input_tokens": 1200,
            "output_tokens": 340,
            "cache_read_tokens": 80,
            "cache_write_tokens": 0,
            "reasoning_tokens": null,
            "requests": 2,
            "cost_usd": null,
            "cost_source": "none"
        }],
        "source": "hook:Stop+transcript",
        "dispatch_run_id": null,
        "repo": "Josh-Archer/agent-meter"
    });
    fs::write(inbox.join("event.json"), serde_json::to_vec(&event).unwrap()).unwrap();

    let state_file = runtime.join("agent-meter").join("state.json");
    let daemon = Command::new(debug_bin("agent-meterd"))
        .args([
            "--once",
            "--config",
            sources.to_str().unwrap(),
            "--state",
            state_file.to_str().unwrap(),
        ])
        .env("HOME", &home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", &config_home)
        .output()
        .expect("run agent-meterd");
    assert!(
        daemon.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&daemon.stderr)
    );
    let stderr = String::from_utf8_lossy(&daemon.stderr);
    assert!(
        !stderr.contains("listening"),
        "default daemon must not open the listener: {stderr}"
    );
    assert!(root.join("processed").join("event.json").is_file());
    assert!(!inbox.join("event.json").exists());

    let cli = Command::new(debug_bin("agent-meter"))
        .args(["usage", "--json", "--since", "24h", "--by", "harness"])
        .env("HOME", &home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", &config_home)
        .output()
        .expect("run agent-meter usage");
    assert!(
        cli.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&cli.stdout).unwrap();
    assert_eq!(report["schema"], "agent-meter.usage-report/v1");
    assert_eq!(report["sessions"], 1);
    assert_eq!(report["group_by"], "harness");
    assert_eq!(report["totals"]["input_tokens"], 1200);
    assert_eq!(report["totals"]["output_tokens"], 340);
    assert!(report["totals"]["reasoning_tokens"].is_null());
    assert_eq!(report["groups"][0]["key"], "claude");

    let quota = fs::read_to_string(&state_file).unwrap();
    let quota: serde_json::Value = serde_json::from_str(&quota).unwrap();
    assert_eq!(quota["version"], 1);
    assert!(quota.get("usage").is_none());
}
