use agent_meter::{
    runtime_state_path, write_state, AgentMeterState, ProviderState, UsageWindow, STATE_VERSION,
};
use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use clap::Parser;
use serde::Deserialize;
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
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(CommandError::Timeout);
                }
                thread::sleep(poll_interval);
                poll_interval = (poll_interval * 2).min(max_poll_interval);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
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

fn refresh(config: &Config) -> AgentMeterState {
    let providers = config
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| match source {
            Source::Mock { provider } => provider.clone(),
            Source::File { path } => match std::fs::read_to_string(path)
                .ok()
                .and_then(|raw| serde_json::from_str::<ProviderState>(&raw).ok())
            {
                Some(provider) => provider,
                None => unavailable(
                    &format!("source-{index}"),
                    "File adapter could not read normalized state",
                ),
            },
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
                        serde_json::from_slice::<ProviderState>(&output.stdout).unwrap_or_else(
                            |_| {
                                unavailable(
                                    program,
                                    "Adapter did not emit a ProviderState JSON document",
                                )
                            },
                        )
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
    loop {
        let state = refresh(&config);
        write_state(&state_path, &state)?;
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
            windows: vec![],
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
}
