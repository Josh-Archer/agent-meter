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

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_refresh_seconds")]
    refresh_seconds: u64,
    #[serde(default = "default_sources")]
    sources: Vec<Source>,
}

fn default_refresh_seconds() -> u64 {
    60
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
            sources: default_sources(),
        });
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
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
            Source::Command { program, args } => {
                let output = Command::new(program).args(args).output();
                match output {
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
                    Err(_) => unavailable(program, "Adapter executable is unavailable"),
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
