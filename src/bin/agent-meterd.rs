use agent_meter::{
    runtime_state_path, write_state, AgentMeterState, ProviderState, UsageWindow, STATE_VERSION,
};
use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use clap::Parser;
use serde::Deserialize;
use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration as StdDuration;

#[derive(Debug, Parser)]
#[command(about = "Refresh the credential-free Agent Meter state file")]
struct Args {
    /// Write a single refresh and exit.
    #[arg(long, conflicts_with = "watch")]
    once: bool,
    /// Refresh indefinitely; intended for a systemd --user service.
    #[arg(long)]
    watch: bool,
    /// Source configuration. Defaults to $XDG_CONFIG_HOME/agent-meter/sources.json.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Destination state file. Defaults to $XDG_RUNTIME_DIR/agent-meter/state.json.
    #[arg(long)]
    state: Option<PathBuf>,
}

fn default_refresh_seconds() -> u64 {
    60
}

fn default_command_timeout_seconds() -> u64 {
    30
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_refresh_seconds")]
    refresh_seconds: u64,
    #[serde(
        default = "default_command_timeout_seconds",
        alias = "timeout_seconds",
        alias = "timeout"
    )]
    command_timeout_seconds: u64,
    #[serde(default = "default_sources")]
    sources: Vec<Source>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Source {
    /// Demonstration data. It is never enabled from a user config by default.
    Mock { provider: ProviderState },
    /// Run a fixed executable directly, never through a shell. stdout must be
    /// a normalized ProviderState JSON document.
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, alias = "timeout")]
        timeout_seconds: Option<u64>,
    },
    /// Read a normalized ProviderState JSON document from disk.
    File { path: PathBuf },
}

fn default_sources() -> Vec<Source> {
    let now = Utc::now();
    vec![Source::Mock {
        provider: ProviderState {
            id: "codex".into(),
            label: "Codex (demo)".into(),
            icon: "codex".into(),
            windows: vec![
                window(
                    "five_hour",
                    "5 hours",
                    72.0,
                    now + Duration::hours(1),
                    "in 1h",
                ),
                window(
                    "weekly",
                    "Weekly",
                    51.0,
                    now + Duration::days(4),
                    "Tue 10:00",
                ),
            ],
            status: "stale".into(),
            detail: Some("Demo data: add sources.json to connect a local adapter.".into()),
            usage_url: Some("https://chatgpt.com/#settings/usage".into()),
        },
    }]
}

fn window(
    id: &str,
    label: &str,
    remaining: f32,
    reset: chrono::DateTime<Utc>,
    reset_label: &str,
) -> UsageWindow {
    UsageWindow {
        id: id.into(),
        label: label.into(),
        remaining_percent: remaining,
        resets_at: Some(reset),
        reset_label: Some(reset_label.into()),
    }
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agent-meter/sources.json")
}

fn load_config(path: &PathBuf) -> Result<Config> {
    if !path.exists() {
        return Ok(Config {
            refresh_seconds: default_refresh_seconds(),
            command_timeout_seconds: default_command_timeout_seconds(),
            sources: default_sources(),
        });
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

#[derive(Debug)]
enum CommandError {
    Unavailable,
    Timeout,
}

fn kill_process_group(child: &mut std::process::Child) {
    let pid = child.id() as libc::pid_t;
    if pid > 1 {
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn run_command_with_timeout(
    program: &str,
    args: &[String],
    timeout: StdDuration,
) -> Result<std::process::Output, CommandError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|_| CommandError::Unavailable)?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let stdout_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stream) = stdout_pipe.take() {
            use std::io::Read;
            let _ = stream.read_to_end(&mut buf);
        }
        buf
    });

    let stderr_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stream) = stderr_pipe.take() {
            use std::io::Read;
            let _ = stream.read_to_end(&mut buf);
        }
        buf
    });

    let start = std::time::Instant::now();
    let mut poll_interval = StdDuration::from_millis(5);
    let max_poll_interval = StdDuration::from_millis(50);

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    kill_process_group(&mut child);
                    let _ = child.wait();
                    // Reader threads are detached when dropped without joining
                    return Err(CommandError::Timeout);
                }
                thread::sleep(poll_interval);
                poll_interval = (poll_interval * 2).min(max_poll_interval);
            }
            Err(_) => {
                kill_process_group(&mut child);
                let _ = child.wait();
                return Err(CommandError::Unavailable);
            }
        }
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn deduplicate_provider_ids(providers: &mut [ProviderState]) {
    let mut seen = HashSet::new();
    for provider in providers.iter_mut() {
        if !seen.insert(provider.id.clone()) {
            let base = provider.id.clone();
            let mut count = 2;
            loop {
                let candidate = format!("{base}-{count}");
                if seen.insert(candidate.clone()) {
                    provider.id = candidate;
                    break;
                }
                count += 1;
            }
        }
    }
}

fn sanitize_state(state: &mut AgentMeterState) {
    state.version = STATE_VERSION;
    for (index, provider) in state.providers.iter_mut().enumerate() {
        if let Err(err) = provider.validate() {
            let hint = if !provider.id.is_empty() {
                provider.id.clone()
            } else {
                format!("provider-{index}")
            };
            *provider = unavailable(&hint, &format!("Invalid provider document: {err}"));
        }
    }
    deduplicate_provider_ids(&mut state.providers);
}

fn refresh(config: &Config) -> AgentMeterState {
    let mut providers: Vec<ProviderState> = config
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| match source {
            Source::Mock { provider } => {
                if let Err(err) = provider.validate() {
                    let hint = if !provider.id.is_empty() {
                        provider.id.as_str()
                    } else {
                        "mock"
                    };
                    unavailable(hint, &format!("Invalid provider document: {err}"))
                } else {
                    provider.clone()
                }
            }
            Source::File { path } => {
                let fallback_hint = format!("source-{index}");
                match std::fs::read_to_string(path) {
                    Ok(raw) => match serde_json::from_str::<ProviderState>(&raw) {
                        Ok(provider) => {
                            if let Err(err) = provider.validate() {
                                let hint = if !provider.id.is_empty() {
                                    provider.id.as_str()
                                } else {
                                    &fallback_hint
                                };
                                unavailable(hint, &format!("Invalid provider document: {err}"))
                            } else {
                                provider
                            }
                        }
                        Err(_) => unavailable(
                            &fallback_hint,
                            "File adapter could not read normalized state",
                        ),
                    },
                    Err(_) => unavailable(
                        &fallback_hint,
                        "File adapter could not read normalized state",
                    ),
                }
            }
            Source::Command {
                program,
                args,
                timeout_seconds,
            } => {
                let secs = timeout_seconds.unwrap_or(config.command_timeout_seconds);
                let secs = if secs == 0 {
                    default_command_timeout_seconds()
                } else {
                    secs
                };
                let timeout = StdDuration::from_secs(secs);
                match run_command_with_timeout(program, args, timeout) {
                    Ok(output) if output.status.success() => {
                        match serde_json::from_slice::<ProviderState>(&output.stdout) {
                            Ok(provider) => {
                                if let Err(err) = provider.validate() {
                                    let hint = if !provider.id.is_empty() {
                                        provider.id.as_str()
                                    } else {
                                        program.as_str()
                                    };
                                    unavailable(hint, &format!("Invalid provider document: {err}"))
                                } else {
                                    provider
                                }
                            }
                            Err(_) => unavailable(
                                program,
                                "Adapter did not emit a ProviderState JSON document",
                            ),
                        }
                    }
                    Ok(_) => unavailable(program, "Adapter exited unsuccessfully"),
                    Err(CommandError::Timeout) => unavailable(program, "Adapter timed out"),
                    Err(CommandError::Unavailable) => {
                        unavailable(program, "Adapter executable is unavailable")
                    }
                }
            }
        })
        .collect();

    deduplicate_provider_ids(&mut providers);

    AgentMeterState {
        version: STATE_VERSION,
        generated_at: Utc::now(),
        providers,
    }
}

fn unavailable(id_hint: &str, detail: &str) -> ProviderState {
    let normalized: String = id_hint
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let id = normalized.trim_matches('-');
    let id = if id.is_empty() { "adapter" } else { id }.to_owned();
    ProviderState {
        id,
        label: id_hint.into(),
        icon: "generic".into(),
        windows: vec![UsageWindow {
            id: "availability".into(),
            label: "Availability".into(),
            remaining_percent: 0.0,
            resets_at: None,
            reset_label: Some("unavailable".into()),
        }],
        status: "unavailable".into(),
        detail: Some(detail.into()),
        usage_url: None,
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_path = args.config.unwrap_or_else(config_path);
    let state_path = args.state.unwrap_or_else(runtime_state_path);
    let config = load_config(&config_path)?;
    if config.refresh_seconds == 0 {
        anyhow::bail!("refresh_seconds must be greater than zero");
    }
    let mut last_good_state: Option<AgentMeterState> = None;
    loop {
        let mut state = refresh(&config);
        if let Err(validation_err) = state.validate() {
            eprintln!(
                "agent-meterd: state validation failed: {validation_err}; writing sanitized state"
            );
            sanitize_state(&mut state);
        }

        match write_state(&state_path, &state) {
            Ok(()) => {
                last_good_state = Some(state);
            }
            Err(err) => {
                eprintln!("agent-meterd: failed to write state: {err}");
                if let Some(good) = &last_good_state {
                    if let Err(e) = write_state(&state_path, good) {
                        eprintln!("agent-meterd: failed to write last good state: {e}");
                    }
                } else if !args.watch {
                    return Err(err);
                }
            }
        }

        if !args.watch {
            break;
        }
        thread::sleep(StdDuration::from_secs(config.refresh_seconds));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_timeout_kills_hung_process() {
        let t0 = std::time::Instant::now();
        let result =
            run_command_with_timeout("sleep", &["10".to_string()], StdDuration::from_millis(50));
        let elapsed = t0.elapsed();
        assert!(matches!(result, Err(CommandError::Timeout)));
        assert!(elapsed < StdDuration::from_secs(2));
    }

    #[test]
    fn command_timeout_kills_process_group_including_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > '{}' && wait", pid_file.display());

        let t0 = std::time::Instant::now();
        let result = run_command_with_timeout(
            "sh",
            &["-c".to_string(), script],
            StdDuration::from_millis(300),
        );
        let elapsed = t0.elapsed();

        assert!(matches!(result, Err(CommandError::Timeout)));
        assert!(
            elapsed < StdDuration::from_secs(2),
            "Expected timeout within ~2s, took {:?}",
            elapsed
        );

        let pid_str =
            std::fs::read_to_string(&pid_file).expect("grandchild pid file should be written");
        let grandchild_pid: libc::pid_t = pid_str.trim().parse().expect("valid grandchild pid");
        assert!(grandchild_pid > 0);

        // Confirm the grandchild process is terminated and not surviving.
        let check_start = std::time::Instant::now();
        let mut surviving = true;
        while check_start.elapsed() < StdDuration::from_millis(1000) {
            let ret = unsafe { libc::kill(grandchild_pid, 0) };
            if ret != 0 {
                surviving = false;
                break;
            }
            if let Ok(status) = std::fs::read_to_string(format!("/proc/{grandchild_pid}/status")) {
                if status
                    .lines()
                    .any(|l| l.starts_with("State:") && l.contains('Z'))
                {
                    surviving = false;
                    break;
                }
            } else {
                surviving = false;
                break;
            }
            thread::sleep(StdDuration::from_millis(20));
        }

        if surviving {
            let _ = unsafe { libc::kill(grandchild_pid, libc::SIGKILL) };
            panic!("Grandchild process {grandchild_pid} survived after timeout");
        }
    }

    #[test]
    fn command_timeout_with_grandchild_pipe_returns_within_two_seconds() {
        let t0 = std::time::Instant::now();
        let result = run_command_with_timeout(
            "sh",
            &["-c".to_string(), "sleep 30 & wait".to_string()],
            StdDuration::from_millis(300),
        );
        let elapsed = t0.elapsed();
        assert!(matches!(result, Err(CommandError::Timeout)));
        assert!(
            elapsed < StdDuration::from_secs(2),
            "Expected timeout within ~2s, took {:?}",
            elapsed
        );
    }

    #[test]
    fn command_success_returns_output() {
        let result =
            run_command_with_timeout("echo", &["hello".to_string()], StdDuration::from_secs(2));
        let output = result.expect("echo should succeed");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[test]
    fn command_unavailable_on_missing_executable() {
        let result = run_command_with_timeout(
            "/nonexistent/binary/for/test",
            &[],
            StdDuration::from_secs(1),
        );
        assert!(matches!(result, Err(CommandError::Unavailable)));
    }

    #[test]
    fn refresh_handles_hung_command_source() {
        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 1,
            sources: vec![Source::Command {
                program: "sleep".into(),
                args: vec!["5".into()],
                timeout_seconds: Some(1),
            }],
        };
        let t0 = std::time::Instant::now();
        let state = refresh(&config);
        assert!(t0.elapsed() < StdDuration::from_secs(3));
        assert_eq!(state.providers.len(), 1);
        let provider = &state.providers[0];
        assert_eq!(provider.status, "unavailable");
        assert_eq!(provider.detail.as_deref(), Some("Adapter timed out"));
    }

    #[test]
    fn refresh_handles_successful_command_source() {
        let valid_json = serde_json::to_string(&ProviderState {
            id: "test-provider".into(),
            label: "Test Provider".into(),
            icon: "test".into(),
            windows: vec![UsageWindow {
                id: "daily".into(),
                label: "Daily".into(),
                remaining_percent: 50.0,
                resets_at: None,
                reset_label: None,
            }],
            status: "fresh".into(),
            detail: None,
            usage_url: None,
        })
        .unwrap();

        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            sources: vec![Source::Command {
                program: "sh".into(),
                args: vec!["-c".into(), format!("echo '{valid_json}'")],
                timeout_seconds: None,
            }],
        };
        let state = refresh(&config);
        assert_eq!(state.providers.len(), 1);
        assert!(state.validate().is_ok());
        let provider = &state.providers[0];
        assert_eq!(provider.id, "test-provider");
        assert_eq!(provider.status, "fresh");
    }

    #[test]
    fn refresh_handles_invalid_json_command_source() {
        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            sources: vec![Source::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "echo not-json".into()],
                timeout_seconds: None,
            }],
        };
        let state = refresh(&config);
        assert_eq!(state.providers.len(), 1);
        let provider = &state.providers[0];
        assert_eq!(provider.status, "unavailable");
        assert_eq!(
            provider.detail.as_deref(),
            Some("Adapter did not emit a ProviderState JSON document")
        );
    }

    #[test]
    fn config_deserialization_defaults() {
        let json = r#"{"sources": [{"kind": "command", "program": "test"}]}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.refresh_seconds, 60);
        assert_eq!(config.command_timeout_seconds, 30);
        match &config.sources[0] {
            Source::Command {
                timeout_seconds, ..
            } => assert_eq!(*timeout_seconds, None),
            _ => panic!("Expected Source::Command"),
        }

        let json_with_timeout = r#"{"timeout_seconds": 15, "sources": [{"kind": "command", "program": "test", "timeout": 5}]}"#;
        let config: Config = serde_json::from_str(json_with_timeout).unwrap();
        assert_eq!(config.command_timeout_seconds, 15);
        match &config.sources[0] {
            Source::Command {
                timeout_seconds, ..
            } => assert_eq!(*timeout_seconds, Some(5)),
            _ => panic!("Expected Source::Command"),
        }
    }

    #[test]
    fn refresh_keeps_good_provider_when_one_document_is_malformed() {
        let good_provider = ProviderState {
            id: "good-provider".into(),
            label: "Good Provider".into(),
            icon: "good".into(),
            windows: vec![UsageWindow {
                id: "daily".into(),
                label: "Daily".into(),
                remaining_percent: 75.0,
                resets_at: None,
                reset_label: None,
            }],
            status: "fresh".into(),
            detail: None,
            usage_url: None,
        };

        // A malformed provider document: remaining_percent exceeds 100.
        let malformed_json = r#"{
            "id": "bad-provider",
            "label": "Bad Provider",
            "icon": "bad",
            "windows": [{
                "id": "daily",
                "label": "Daily",
                "remaining_percent": 150.0
            }],
            "status": "fresh"
        }"#;

        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            sources: vec![
                Source::Mock {
                    provider: good_provider,
                },
                Source::Command {
                    program: "sh".into(),
                    args: vec!["-c".into(), format!("echo '{malformed_json}'")],
                    timeout_seconds: None,
                },
            ],
        };

        let state = refresh(&config);
        assert_eq!(state.providers.len(), 2);
        assert!(state.validate().is_ok());

        let good = state
            .providers
            .iter()
            .find(|p| p.id == "good-provider")
            .expect("good provider should be present");
        assert_eq!(good.status, "fresh");
        assert_eq!(good.windows[0].remaining_percent, 75.0);

        let bad = state
            .providers
            .iter()
            .find(|p| p.id == "bad-provider")
            .expect("bad provider should be replaced with unavailable entry");
        assert_eq!(bad.status, "unavailable");
        assert!(
            bad.detail
                .as_deref()
                .unwrap_or_default()
                .contains("Invalid provider document"),
            "Expected detail to mention invalid provider document, got: {:?}",
            bad.detail
        );
    }

    #[test]
    fn refresh_two_failed_commands_avoids_duplicate_id_validation_failure() {
        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            sources: vec![
                Source::Command {
                    program: "false".into(),
                    args: vec![],
                    timeout_seconds: None,
                },
                Source::Command {
                    program: "false".into(),
                    args: vec![],
                    timeout_seconds: None,
                },
            ],
        };

        let state = refresh(&config);
        assert_eq!(state.providers.len(), 2);
        assert!(
            state.validate().is_ok(),
            "State should validate successfully with two failed commands, but got error: {:?}",
            state.validate().err()
        );
        assert_ne!(state.providers[0].id, state.providers[1].id);
        assert_eq!(state.providers[0].status, "unavailable");
        assert_eq!(state.providers[1].status, "unavailable");
    }

    #[test]
    fn refresh_handles_malformed_file_provider() {
        let dir = tempfile::tempdir().unwrap();
        let bad_file = dir.path().join("bad.json");
        std::fs::write(
            &bad_file,
            r#"{"id": "bad", "label": "Bad", "icon": "b", "windows": [], "status": "fresh"}"#,
        )
        .unwrap();

        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            sources: vec![Source::File { path: bad_file }],
        };

        let state = refresh(&config);
        assert_eq!(state.providers.len(), 1);
        assert!(state.validate().is_ok());
        let provider = &state.providers[0];
        assert_eq!(provider.status, "unavailable");
        assert!(provider
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("Invalid provider document"));
    }

    #[test]
    fn sanitize_state_fixes_invalid_providers_and_duplicate_ids() {
        let mut state = AgentMeterState {
            version: 99,
            generated_at: Utc::now(),
            providers: vec![
                ProviderState {
                    id: "duplicate".into(),
                    label: "First".into(),
                    icon: "icon".into(),
                    windows: vec![],
                    status: "fresh".into(),
                    detail: None,
                    usage_url: None,
                },
                ProviderState {
                    id: "duplicate".into(),
                    label: "Second".into(),
                    icon: "icon".into(),
                    windows: vec![UsageWindow {
                        id: "win".into(),
                        label: "Win".into(),
                        remaining_percent: 50.0,
                        resets_at: None,
                        reset_label: None,
                    }],
                    status: "fresh".into(),
                    detail: None,
                    usage_url: None,
                },
            ],
        };

        assert!(state.validate().is_err());
        sanitize_state(&mut state);
        assert!(state.validate().is_ok());
        assert_eq!(state.version, STATE_VERSION);
        assert_eq!(state.providers.len(), 2);
        assert_ne!(state.providers[0].id, state.providers[1].id);
    }
}
