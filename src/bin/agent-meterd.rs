use agent_meter::{
    forecast, runtime_state_path, write_state, AgentMeterState, ProviderState, UsageWindow,
    STATE_VERSION,
};
use agent_meter::forecast::NotifySink;
use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use clap::Parser;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration as StdDuration, Instant};

#[derive(Debug, Parser)]
#[command(about = "Refresh the credential-free Agent Meter state file")]
struct Args {
    /// Write a single refresh and exit.
    #[arg(long, conflicts_with = "watch")]
    once: bool,
    /// Refresh indefinitely. The systemd user unit is agent-meter.service;
    /// this binary is agent-meterd (`systemctl --user restart agent-meter.service`).
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

fn default_max_observations() -> usize {
    forecast::DEFAULT_MAX_OBSERVATIONS
}

fn default_stale_after_seconds() -> i64 {
    forecast::DEFAULT_STALE_AFTER_SECS
}

/// Opt-in quota-exhaustion forecasting. Disabled by default: set
/// `enabled = true` to record the bounded local usage history, and list
/// `notify_thresholds` to also receive threshold notifications.
#[derive(Debug, Clone, Deserialize)]
struct ForecastConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_max_observations")]
    max_observations_per_window: usize,
    #[serde(default = "default_stale_after_seconds")]
    stale_after_seconds: i64,
    /// Remaining-percent thresholds that trigger a notification, for example
    /// `[25.0, 10.0, 5.0]`. Empty disables notifications.
    #[serde(default)]
    notify_thresholds: Vec<f32>,
    /// Override for the usage-history file. Defaults to
    /// `$XDG_STATE_HOME/agent-meter/usage-history.json`.
    #[serde(default)]
    history_path: Option<PathBuf>,
    /// Override for the notifications log. Defaults to
    /// `$XDG_STATE_HOME/agent-meter/notifications.jsonl`.
    #[serde(default)]
    notifications_path: Option<PathBuf>,
}

impl Default for ForecastConfig {
    fn default() -> Self {
        ForecastConfig {
            enabled: false,
            max_observations_per_window: default_max_observations(),
            stale_after_seconds: default_stale_after_seconds(),
            notify_thresholds: Vec::new(),
            history_path: None,
            notifications_path: None,
        }
    }
}

/// Enough overlap for the usual adapter set without stacking every CLI at once.
fn default_max_concurrent_refreshes() -> usize {
    4
}

/// One parallel wave, a little past the default per-command timeout.
/// Sequential refreshes would otherwise add every adapter's timeout together.
fn default_refresh_deadline_seconds() -> u64 {
    45
}

const GLOBAL_DEADLINE_DETAIL: &str = "Refresh exceeded the global deadline";

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
    #[serde(default = "default_max_concurrent_refreshes")]
    max_concurrent_refreshes: usize,
    #[serde(default = "default_refresh_deadline_seconds")]
    refresh_deadline_seconds: u64,
    #[serde(default = "default_sources")]
    sources: Vec<Source>,
    #[serde(default)]
    forecast: ForecastConfig,
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
            updated_at: None,
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
            max_concurrent_refreshes: default_max_concurrent_refreshes(),
            refresh_deadline_seconds: default_refresh_deadline_seconds(),
            sources: default_sources(),
            forecast: ForecastConfig::default(),
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

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

fn kill_process_group(pgid: libc::pid_t, child: &mut std::process::Child) {
    if pgid > 1 {
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn read_bounded_pipe<R: std::io::Read>(mut stream: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut stream) = stream.take() {
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() < MAX_OUTPUT_BYTES {
                        let to_take = (MAX_OUTPUT_BYTES - buf.len()).min(n);
                        buf.extend_from_slice(&chunk[..to_take]);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }
    buf
}

fn clean_up_reader_threads(
    stdout_thread: thread::JoinHandle<Vec<u8>>,
    stderr_thread: thread::JoinHandle<Vec<u8>>,
) {
    let join_deadline = std::time::Instant::now() + StdDuration::from_millis(200);
    while (!stdout_thread.is_finished() || !stderr_thread.is_finished())
        && std::time::Instant::now() < join_deadline
    {
        thread::sleep(StdDuration::from_millis(5));
    }
    if stdout_thread.is_finished() {
        let _ = stdout_thread.join();
    }
    if stderr_thread.is_finished() {
        let _ = stderr_thread.join();
    }
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

    let pgid = child.id() as libc::pid_t;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_thread = thread::spawn(move || read_bounded_pipe(stdout_pipe));
    let stderr_thread = thread::spawn(move || read_bounded_pipe(stderr_pipe));

    let start = std::time::Instant::now();
    let mut poll_interval = StdDuration::from_millis(5);
    let max_poll_interval = StdDuration::from_millis(50);

    let mut exit_status: Option<std::process::ExitStatus> = None;

    loop {
        if exit_status.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => exit_status = Some(status),
                Ok(None) => {}
                Err(_) => {
                    kill_process_group(pgid, &mut child);
                    let _ = child.wait();
                    clean_up_reader_threads(stdout_thread, stderr_thread);
                    return Err(CommandError::Unavailable);
                }
            }
        }

        if exit_status.is_some() && stdout_thread.is_finished() && stderr_thread.is_finished() {
            break;
        }

        if start.elapsed() >= timeout {
            kill_process_group(pgid, &mut child);
            let _ = child.wait();
            clean_up_reader_threads(stdout_thread, stderr_thread);
            return Err(CommandError::Timeout);
        }

        let remaining = timeout.saturating_sub(start.elapsed());
        thread::sleep(poll_interval.min(remaining));
        poll_interval = (poll_interval * 2).min(max_poll_interval);
    }

    let status = exit_status.expect("exit status must be set on loop exit");
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

enum SourceRefresh {
    /// A normalized document from the source, which may itself be unavailable.
    Document(ProviderState),
    /// The source produced no document. `detail` is the failure reason.
    Failed { hint: String, detail: String },
}

fn effective_concurrency(config: &Config) -> usize {
    let configured = if config.max_concurrent_refreshes == 0 {
        default_max_concurrent_refreshes()
    } else {
        config.max_concurrent_refreshes
    };
    configured.max(1).min(config.sources.len())
}

fn refresh_deadline_instant(config: &Config) -> Instant {
    let secs = if config.refresh_deadline_seconds == 0 {
        default_refresh_deadline_seconds()
    } else {
        config.refresh_deadline_seconds
    };
    Instant::now() + StdDuration::from_secs(secs)
}

fn per_source_timeout(timeout_seconds: Option<u64>, config: &Config) -> StdDuration {
    let secs = timeout_seconds.unwrap_or(config.command_timeout_seconds);
    let secs = if secs == 0 {
        default_command_timeout_seconds()
    } else {
        secs
    };
    StdDuration::from_secs(secs)
}

fn source_hint(index: usize, source: &Source) -> String {
    match source {
        Source::Mock { provider } => {
            if provider.id.is_empty() {
                "mock".into()
            } else {
                provider.id.clone()
            }
        }
        Source::File { .. } => format!("source-{index}"),
        Source::Command { program, .. } => program.clone(),
    }
}

fn keeps_last_good(provider: &ProviderState) -> bool {
    provider.status != "unavailable" && provider.status != "error"
}

fn stamp_updated_at(provider: &mut ProviderState) {
    if provider.updated_at.is_none() {
        provider.updated_at = Some(Utc::now());
    }
}

fn mark_stale(mut provider: ProviderState, reason: &str) -> ProviderState {
    let stamp = match provider.updated_at {
        Some(ts) => ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        None => "an earlier refresh".to_owned(),
    };
    provider.status = "stale".into();
    provider.detail = Some(format!("Stale: last good data at {stamp} ({reason})"));
    provider
}

fn provider_or_invalid(provider: ProviderState, fallback_hint: &str) -> SourceRefresh {
    match provider.validate() {
        Ok(()) => SourceRefresh::Document(provider),
        Err(err) => {
            let hint = if !provider.id.is_empty() {
                provider.id
            } else {
                fallback_hint.to_owned()
            };
            SourceRefresh::Failed {
                hint,
                detail: format!("Invalid provider document: {err}"),
            }
        }
    }
}

fn refresh_source(
    index: usize,
    source: &Source,
    config: &Config,
    deadline: Instant,
) -> SourceRefresh {
    if Instant::now() >= deadline {
        return SourceRefresh::Failed {
            hint: source_hint(index, source),
            detail: GLOBAL_DEADLINE_DETAIL.into(),
        };
    }
    match source {
        Source::Mock { provider } => provider_or_invalid(provider.clone(), "mock"),
        Source::File { path } => {
            let fallback_hint = format!("source-{index}");
            match std::fs::read_to_string(path) {
                Ok(raw) => match serde_json::from_str::<ProviderState>(&raw) {
                    Ok(provider) => provider_or_invalid(provider, &fallback_hint),
                    Err(_) => SourceRefresh::Failed {
                        hint: fallback_hint,
                        detail: "File adapter could not read normalized state".into(),
                    },
                },
                Err(_) => SourceRefresh::Failed {
                    hint: fallback_hint,
                    detail: "File adapter could not read normalized state".into(),
                },
            }
        }
        Source::Command {
            program,
            args,
            timeout_seconds,
        } => {
            let per_source = per_source_timeout(*timeout_seconds, config);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return SourceRefresh::Failed {
                    hint: program.clone(),
                    detail: GLOBAL_DEADLINE_DETAIL.into(),
                };
            }
            let capped_by_deadline = remaining < per_source;
            let timeout = if capped_by_deadline {
                remaining
            } else {
                per_source
            };
            match run_command_with_timeout(program, args, timeout) {
                Ok(output) if output.status.success() => {
                    match serde_json::from_slice::<ProviderState>(&output.stdout) {
                        Ok(provider) => provider_or_invalid(provider, program),
                        Err(_) => SourceRefresh::Failed {
                            hint: program.clone(),
                            detail: "Adapter did not emit a ProviderState JSON document".into(),
                        },
                    }
                }
                Ok(_) => SourceRefresh::Failed {
                    hint: program.clone(),
                    detail: "Adapter exited unsuccessfully".into(),
                },
                Err(CommandError::Timeout) if capped_by_deadline => SourceRefresh::Failed {
                    hint: program.clone(),
                    detail: GLOBAL_DEADLINE_DETAIL.into(),
                },
                Err(CommandError::Timeout) => SourceRefresh::Failed {
                    hint: program.clone(),
                    detail: "Adapter timed out".into(),
                },
                Err(CommandError::Unavailable) => SourceRefresh::Failed {
                    hint: program.clone(),
                    detail: "Adapter executable is unavailable".into(),
                },
            }
        }
    }
}

fn record_source(
    index: usize,
    refreshed: SourceRefresh,
    slots: &mut [Option<ProviderState>],
    last_good: &mut HashMap<usize, ProviderState>,
) {
    let provider = match refreshed {
        SourceRefresh::Document(mut provider) => {
            if keeps_last_good(&provider) {
                stamp_updated_at(&mut provider);
                last_good.insert(index, provider.clone());
            } else {
                last_good.remove(&index);
            }
            provider
        }
        SourceRefresh::Failed { hint, detail } => {
            if let Some(previous) = last_good.get(&index) {
                let stale = mark_stale(previous.clone(), &detail);
                last_good.insert(index, stale.clone());
                stale
            } else {
                unavailable(&hint, &detail)
            }
        }
    };
    slots[index] = Some(provider);
}

fn snapshot_from(
    slots: &[Option<ProviderState>],
    last_good: &HashMap<usize, ProviderState>,
    include_inflight: bool,
) -> AgentMeterState {
    let mut providers = Vec::with_capacity(slots.len());
    for (index, slot) in slots.iter().enumerate() {
        if let Some(provider) = slot {
            providers.push(provider.clone());
        } else if include_inflight {
            if let Some(provider) = last_good.get(&index) {
                providers.push(provider.clone());
            }
        }
    }
    deduplicate_provider_ids(&mut providers);
    AgentMeterState {
        version: STATE_VERSION,
        generated_at: Utc::now(),
        providers,
    }
}

#[cfg(test)]
fn refresh(config: &Config) -> AgentMeterState {
    let mut last_good = HashMap::new();
    refresh_publishing(config, &mut last_good, |_| {})
}

/// Refresh every source with at most `max_concurrent_refreshes` in flight.
///
/// `publish` runs as soon as a source settles, with source order preserved.
/// Providers still running keep their previous last-good snapshot. The wave
/// stops at `refresh_deadline_seconds` even when a per-source timeout is longer.
/// `last_good` survives across calls so a later failure can be marked stale
/// without moving that provider's `updated_at`.
fn refresh_publishing(
    config: &Config,
    last_good: &mut HashMap<usize, ProviderState>,
    mut publish: impl FnMut(&AgentMeterState),
) -> AgentMeterState {
    if config.sources.is_empty() {
        let state = AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: Vec::new(),
        };
        publish(&state);
        return state;
    }

    let deadline = refresh_deadline_instant(config);
    let worker_count = effective_concurrency(config);
    // Workers borrow the queue for the whole `thread::scope` lifetime, so it
    // cannot be a local inside the scope closure.
    let (job_tx, job_rx) = mpsc::channel::<usize>();
    let job_queue = std::sync::Mutex::new(job_rx);
    let job_queue = &job_queue;
    let (result_tx, result_rx) = mpsc::channel::<(usize, SourceRefresh)>();

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let result_tx = result_tx.clone();
            scope.spawn(move || loop {
                let next = {
                    let rx = job_queue.lock().unwrap_or_else(|err| err.into_inner());
                    rx.recv()
                };
                let index = match next {
                    Ok(index) => index,
                    Err(_) => break,
                };
                let refreshed = refresh_source(index, &config.sources[index], config, deadline);
                if result_tx.send((index, refreshed)).is_err() {
                    break;
                }
            });
        }
        drop(result_tx);

        for index in 0..config.sources.len() {
            let _ = job_tx.send(index);
        }
        drop(job_tx);

        let mut slots = vec![None; config.sources.len()];
        let mut outstanding = config.sources.len();
        let mut latest = AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: Vec::new(),
        };

        while outstanding > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let received = if remaining.is_zero() {
                result_rx.try_recv().ok()
            } else {
                match result_rx.recv_timeout(remaining) {
                    Ok(item) => Some(item),
                    Err(mpsc::RecvTimeoutError::Timeout) => result_rx.try_recv().ok(),
                    Err(mpsc::RecvTimeoutError::Disconnected) => None,
                }
            };
            let Some((index, refreshed)) = received else {
                break;
            };
            if slots[index].is_some() {
                continue;
            }
            outstanding -= 1;
            record_source(index, refreshed, &mut slots, last_good);
            latest = snapshot_from(&slots, last_good, true);
            publish(&latest);
        }

        if slots.iter().any(|slot| slot.is_none()) {
            for index in 0..slots.len() {
                if slots[index].is_none() {
                    record_source(
                        index,
                        SourceRefresh::Failed {
                            hint: source_hint(index, &config.sources[index]),
                            detail: GLOBAL_DEADLINE_DETAIL.into(),
                        },
                        &mut slots,
                        last_good,
                    );
                }
            }
            latest = snapshot_from(&slots, last_good, false);
            publish(&latest);
        }
        latest
    })
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
        updated_at: None,
    }
}

fn write_stable_json(state: &AgentMeterState) {
    if let Err(e) = agent_meter::write_stable_state(state) {
        eprintln!("agent-meterd: could not write stable JSON: {e}");
    }
}

/// Record the refresh in the bounded usage history and emit any configured
/// threshold notifications. A single call site after `refresh()` so the
/// refresh loop itself stays untouched.
fn update_forecast_history(config: &ForecastConfig, state: &AgentMeterState) {
    let history_path = config
        .history_path
        .clone()
        .unwrap_or_else(forecast::default_history_path);
    let mut history = forecast::load_history(&history_path).unwrap_or_default();
    let now = Utc::now();
    forecast::record_state(
        &mut history,
        state,
        now,
        config.max_observations_per_window,
    );
    if let Err(err) = forecast::save_history(&history_path, &history) {
        eprintln!("agent-meterd: could not write usage history: {err}");
    }
    if config.notify_thresholds.is_empty() {
        return;
    }
    let events = forecast::check_notifications(
        &mut history,
        &config.notify_thresholds,
        now,
        config.stale_after_seconds,
    );
    if events.is_empty() {
        return;
    }
    let sink_path = config
        .notifications_path
        .clone()
        .unwrap_or_else(forecast::default_notifications_path);
    let mut sink = forecast::FileLogSink::new(sink_path);
    for event in &events {
        sink.notify(event);
    }
    // Persist duplicate-suppression state: notifications mutate `notified`.
    if let Err(err) = forecast::save_history(&history_path, &history) {
        eprintln!("agent-meterd: could not persist notification state: {err}");
    }
}

fn emit_state(
    state_path: &std::path::Path,
    state: &AgentMeterState,
    last_good_state: &mut Option<AgentMeterState>,
) -> Result<()> {
    let mut state = state.clone();
    if let Err(validation_err) = state.validate() {
        eprintln!(
            "agent-meterd: state validation failed: {validation_err}; writing sanitized state"
        );
        sanitize_state(&mut state);
    }
    match write_state(state_path, &state) {
        Ok(()) => {
            write_stable_json(&state);
            *last_good_state = Some(state);
            Ok(())
        }
        Err(err) => {
            eprintln!("agent-meterd: failed to write state: {err}");
            if let Some(good) = &last_good_state {
                if let Err(e) = write_state(state_path, good) {
                    eprintln!("agent-meterd: failed to write last good state: {e}");
                }
            }
            Err(err)
        }
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
    let mut last_good: HashMap<usize, ProviderState> = HashMap::new();
    let mut last_good_state: Option<AgentMeterState> = None;
    loop {
        let mut write_error: Option<anyhow::Error> = None;
        let final_state = refresh_publishing(&config, &mut last_good, |state| {
            if let Err(err) = emit_state(&state_path, state, &mut last_good_state) {
                write_error = Some(err);
            }
        });
        if config.forecast.enabled {
            update_forecast_history(&config.forecast, &final_state);
        }
        if last_good_state.is_none() {
            if let Some(err) = write_error {
                if !args.watch {
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
    fn command_timeout_kills_descendants_when_adapter_exits_first() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("descendant.pid");
        let script = format!("sleep 30 & echo $! > '{}'", pid_file.display());

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
            std::fs::read_to_string(&pid_file).expect("descendant pid file should be written");
        let descendant_pid: libc::pid_t = pid_str.trim().parse().expect("valid descendant pid");
        assert!(descendant_pid > 0);

        let check_start = std::time::Instant::now();
        let mut surviving = true;
        while check_start.elapsed() < StdDuration::from_millis(1000) {
            let ret = unsafe { libc::kill(descendant_pid, 0) };
            if ret != 0 {
                surviving = false;
                break;
            }
            if let Ok(status) = std::fs::read_to_string(format!("/proc/{descendant_pid}/status")) {
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
            let _ = unsafe { libc::kill(descendant_pid, libc::SIGKILL) };
            panic!("Descendant process {descendant_pid} survived after timeout");
        }
    }

    #[test]
    fn command_bounds_captured_output_on_large_stream() {
        let t0 = std::time::Instant::now();
        let result = run_command_with_timeout(
            "sh",
            &["-c".to_string(), "head -c 2097152 /dev/zero".to_string()],
            StdDuration::from_secs(5),
        );
        let elapsed = t0.elapsed();
        assert!(elapsed < StdDuration::from_secs(5));
        let output = result.expect("command should succeed");
        assert_eq!(output.stdout.len(), MAX_OUTPUT_BYTES);
    }

    #[test]
    fn command_timeout_bounds_captured_output_on_infinite_stream() {
        let t0 = std::time::Instant::now();
        let result = run_command_with_timeout("yes", &[], StdDuration::from_millis(200));
        let elapsed = t0.elapsed();
        assert!(matches!(result, Err(CommandError::Timeout)));
        assert!(
            elapsed < StdDuration::from_secs(2),
            "Expected timeout within ~2s, took {:?}",
            elapsed
        );
    }

    #[test]
    fn refresh_handles_adapter_exiting_with_inherited_descendant_pipe() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("descendant.pid");
        let script = format!("sleep 30 & echo $! > '{}'", pid_file.display());
        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 1,
            max_concurrent_refreshes: default_max_concurrent_refreshes(),
            refresh_deadline_seconds: default_refresh_deadline_seconds(),
            forecast: ForecastConfig::default(),
            sources: vec![Source::Command {
                program: "sh".into(),
                args: vec!["-c".into(), script],
                timeout_seconds: Some(1),
            }],
        };
        let t0 = std::time::Instant::now();
        let state = refresh(&config);
        let elapsed = t0.elapsed();
        assert!(
            elapsed < StdDuration::from_secs(3),
            "Expected refresh timeout within ~3s, took {:?}",
            elapsed
        );
        assert_eq!(state.providers.len(), 1);
        let provider = &state.providers[0];
        assert_eq!(provider.status, "unavailable");
        assert_eq!(provider.detail.as_deref(), Some("Adapter timed out"));

        let pid_str =
            std::fs::read_to_string(&pid_file).expect("descendant pid file should be written");
        let descendant_pid: libc::pid_t = pid_str.trim().parse().expect("valid descendant pid");
        assert!(descendant_pid > 0);

        let check_start = std::time::Instant::now();
        let mut surviving = true;
        while check_start.elapsed() < StdDuration::from_millis(1000) {
            let ret = unsafe { libc::kill(descendant_pid, 0) };
            if ret != 0 {
                surviving = false;
                break;
            }
            if let Ok(status) = std::fs::read_to_string(format!("/proc/{descendant_pid}/status")) {
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
            let _ = unsafe { libc::kill(descendant_pid, libc::SIGKILL) };
            panic!("Descendant process {descendant_pid} survived after refresh timeout");
        }
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
            max_concurrent_refreshes: default_max_concurrent_refreshes(),
            refresh_deadline_seconds: default_refresh_deadline_seconds(),
            forecast: ForecastConfig::default(),
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
            updated_at: None,
        })
        .unwrap();

        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: default_max_concurrent_refreshes(),
            refresh_deadline_seconds: default_refresh_deadline_seconds(),
            forecast: ForecastConfig::default(),
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
        assert!(provider.updated_at.is_some());
    }

    #[test]
    fn refresh_handles_invalid_json_command_source() {
        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: default_max_concurrent_refreshes(),
            refresh_deadline_seconds: default_refresh_deadline_seconds(),
            forecast: ForecastConfig::default(),
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
        assert_eq!(config.max_concurrent_refreshes, 4);
        assert_eq!(config.refresh_deadline_seconds, 45);
        match &config.sources[0] {
            Source::Command {
                timeout_seconds, ..
            } => assert_eq!(*timeout_seconds, None),
            _ => panic!("Expected Source::Command"),
        }

        let json_with_timeout = r#"{"timeout_seconds": 15, "max_concurrent_refreshes": 2, "refresh_deadline_seconds": 12, "sources": [{"kind": "command", "program": "test", "timeout": 5}]}"#;
        let config: Config = serde_json::from_str(json_with_timeout).unwrap();
        assert_eq!(config.command_timeout_seconds, 15);
        assert_eq!(config.max_concurrent_refreshes, 2);
        assert_eq!(config.refresh_deadline_seconds, 12);
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
            updated_at: None,
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
            max_concurrent_refreshes: default_max_concurrent_refreshes(),
            refresh_deadline_seconds: default_refresh_deadline_seconds(),
            forecast: ForecastConfig::default(),
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
            max_concurrent_refreshes: default_max_concurrent_refreshes(),
            refresh_deadline_seconds: default_refresh_deadline_seconds(),
            forecast: ForecastConfig::default(),
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
            max_concurrent_refreshes: default_max_concurrent_refreshes(),
            refresh_deadline_seconds: default_refresh_deadline_seconds(),
            forecast: ForecastConfig::default(),
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
                    updated_at: None,
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
                    updated_at: None,
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

    fn fresh_provider(id: &str, percent: f32) -> ProviderState {
        ProviderState {
            id: id.into(),
            label: id.into(),
            icon: "generic".into(),
            windows: vec![UsageWindow {
                id: "daily".into(),
                label: "Daily".into(),
                remaining_percent: percent,
                resets_at: None,
                reset_label: None,
            }],
            status: "fresh".into(),
            detail: None,
            usage_url: None,
            updated_at: None,
        }
    }

    fn echo_provider(provider: &ProviderState) -> Source {
        let json = serde_json::to_string(provider).unwrap();
        Source::Command {
            program: "sh".into(),
            args: vec!["-c".into(), format!("echo '{json}'")],
            timeout_seconds: Some(5),
        }
    }

    #[test]
    fn healthy_provider_is_published_before_slow_provider_finishes() {
        let fast = fresh_provider("fast-provider", 80.0);
        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 2,
            refresh_deadline_seconds: 8,
            sources: vec![
                Source::Command {
                    program: "sh".into(),
                    args: vec!["-c".into(), "sleep 2".into()],
                    timeout_seconds: Some(5),
                },
                echo_provider(&fast),
            ],
            forecast: ForecastConfig::default(),
        };
        let started = Instant::now();
        let mut snapshots = Vec::new();
        let mut last_good = HashMap::new();
        let state = refresh_publishing(&config, &mut last_good, |snapshot| {
            snapshots.push((started.elapsed(), snapshot.clone()));
        });

        let early = snapshots.iter().find(|(_, snapshot)| {
            snapshot
                .providers
                .iter()
                .any(|provider| provider.id == "fast-provider" && provider.status == "fresh")
        });
        let (published_at, early_state) = early.expect("fast provider should be published");
        assert!(
            *published_at < StdDuration::from_millis(1200),
            "healthy provider was held until {published_at:?}"
        );
        assert_eq!(early_state.providers.len(), 1);
        assert!(
            started.elapsed() >= StdDuration::from_millis(1500),
            "slow source should still have been refreshed, took {:?}",
            started.elapsed()
        );
        assert!(started.elapsed() < StdDuration::from_secs(5));
        assert_eq!(state.providers.len(), 2);
        assert_eq!(state.providers[1].id, "fast-provider");
        assert_eq!(state.providers[1].status, "fresh");
        assert_eq!(state.providers[0].status, "unavailable");
        assert!(state.validate().is_ok());
    }

    #[test]
    fn global_deadline_stops_the_wave_and_keeps_last_good_timestamp() {
        let fast = fresh_provider("codex", 64.0);
        let success = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 2,
            refresh_deadline_seconds: 5,
            sources: vec![echo_provider(&fast)],
            forecast: ForecastConfig::default(),
        };
        let mut last_good = HashMap::new();
        let first = refresh_publishing(&success, &mut last_good, |_| {});
        let stamp = first.providers[0]
            .updated_at
            .expect("successful refresh records last-good time");
        assert_eq!(first.providers[0].status, "fresh");

        let hung = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 30,
            max_concurrent_refreshes: 2,
            refresh_deadline_seconds: 1,
            sources: vec![
                Source::Command {
                    program: "sleep".into(),
                    args: vec!["30".into()],
                    timeout_seconds: Some(30),
                },
                echo_provider(&fresh_provider("grok", 40.0)),
            ],
            forecast: ForecastConfig::default(),
        };
        // The sleeping source replaces index 0. Keep codex's last-good there.
        let started = Instant::now();
        let mut saw_grok_early = false;
        let state = refresh_publishing(&hung, &mut last_good, |snapshot| {
            let grok_ready = snapshot
                .providers
                .iter()
                .any(|provider| provider.id == "grok" && provider.status == "fresh");
            let codex_settled_stale = snapshot
                .providers
                .iter()
                .any(|provider| provider.id == "codex" && provider.status == "stale");
            if grok_ready
                && !codex_settled_stale
                && started.elapsed() < StdDuration::from_millis(800)
            {
                saw_grok_early = true;
            }
        });
        let elapsed = started.elapsed();

        assert!(
            saw_grok_early,
            "grok should publish while codex is still inside the deadline"
        );
        assert!(
            elapsed < StdDuration::from_secs(3),
            "global deadline should bound the wave, took {elapsed:?}"
        );
        assert!(state.validate().is_ok());
        let codex = state
            .providers
            .iter()
            .find(|provider| provider.id == "codex")
            .expect("last good codex snapshot");
        assert_eq!(codex.status, "stale");
        assert_eq!(codex.updated_at, Some(stamp));
        assert_eq!(codex.windows[0].remaining_percent, 64.0);
        let detail = codex.detail.as_deref().unwrap_or_default();
        let stamp_text = stamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        assert!(
            detail.contains("Stale: last good data at"),
            "detail should mark stale data, got {detail}"
        );
        assert!(
            detail.contains(&stamp_text),
            "detail should include the last-good timestamp {stamp_text}, got {detail}"
        );
        assert!(
            detail.contains(GLOBAL_DEADLINE_DETAIL),
            "detail should name the deadline, got {detail}"
        );

        let output = agent_meter::JsonOutput::from_state(&state);
        let codex_json = output
            .providers
            .iter()
            .find(|provider| provider.provider == "codex")
            .unwrap();
        assert!(codex_json.stale);
        assert!(codex_json.available);
        assert_eq!(codex_json.updated_at, stamp_text);
        let grok = state
            .providers
            .iter()
            .find(|provider| provider.id == "grok")
            .unwrap();
        assert_eq!(grok.status, "fresh");
        assert!(grok.updated_at.unwrap() >= stamp);
        let grok_json = output
            .providers
            .iter()
            .find(|provider| provider.provider == "grok")
            .unwrap();
        assert!(!grok_json.stale);
        assert_eq!(
            grok_json.updated_at,
            grok.updated_at
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
        );
    }

    #[test]
    fn repeated_failure_keeps_the_original_last_good_timestamp() {
        let success = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 1,
            refresh_deadline_seconds: 5,
            sources: vec![echo_provider(&fresh_provider("codex", 70.0))],
            forecast: ForecastConfig::default(),
        };
        let mut last_good = HashMap::new();
        let first = refresh_publishing(&success, &mut last_good, |_| {});
        let stamp = first.providers[0].updated_at.unwrap();

        let failure = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 1,
            refresh_deadline_seconds: 5,
            sources: vec![Source::Command {
                program: "false".into(),
                args: vec![],
                timeout_seconds: None,
            }],
            forecast: ForecastConfig::default(),
        };
        let second = refresh_publishing(&failure, &mut last_good, |_| {});
        assert_eq!(second.providers[0].status, "stale");
        assert_eq!(second.providers[0].updated_at, Some(stamp));
        assert_eq!(second.providers[0].windows[0].remaining_percent, 70.0);

        let third = refresh_publishing(&failure, &mut last_good, |_| {});
        assert_eq!(third.providers[0].status, "stale");
        assert_eq!(third.providers[0].updated_at, Some(stamp));
        assert_eq!(third.providers[0].windows[0].remaining_percent, 70.0);
    }

    #[test]
    fn explicit_unavailable_document_clears_last_good() {
        let mut last_good = HashMap::new();
        let success = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 1,
            refresh_deadline_seconds: 5,
            sources: vec![echo_provider(&fresh_provider("codex", 55.0))],
            forecast: ForecastConfig::default(),
        };
        refresh_publishing(&success, &mut last_good, |_| {});

        let mut unavailable_doc = fresh_provider("codex", 0.0);
        unavailable_doc.status = "unavailable".into();
        unavailable_doc.detail = Some("signed out".into());
        let explicit = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 1,
            refresh_deadline_seconds: 5,
            sources: vec![echo_provider(&unavailable_doc)],
            forecast: ForecastConfig::default(),
        };
        let state = refresh_publishing(&explicit, &mut last_good, |_| {});
        assert_eq!(state.providers[0].status, "unavailable");
        assert_eq!(state.providers[0].detail.as_deref(), Some("signed out"));
        assert!(last_good.is_empty());

        let failure = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 1,
            refresh_deadline_seconds: 5,
            sources: vec![Source::Command {
                program: "false".into(),
                args: vec![],
                timeout_seconds: None,
            }],
            forecast: ForecastConfig::default(),
        };
        let after = refresh_publishing(&failure, &mut last_good, |_| {});
        assert_eq!(after.providers[0].status, "unavailable");
        assert_eq!(
            after.providers[0].detail.as_deref(),
            Some("Adapter exited unsuccessfully")
        );
    }

    #[test]
    fn refresh_limits_concurrent_command_sources() {
        let dir = tempfile::tempdir().unwrap();
        let started = dir.path().join("started");
        std::fs::create_dir(&started).unwrap();
        let release = dir.path().join("release");
        let script = format!(
            "touch \"{}/$$\"; while [ ! -f \"{}\" ]; do sleep 0.05; done",
            started.display(),
            release.display()
        );
        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 10,
            max_concurrent_refreshes: 2,
            refresh_deadline_seconds: 10,
            sources: (0..4)
                .map(|_| Source::Command {
                    program: "sh".into(),
                    args: vec!["-c".into(), script.clone()],
                    timeout_seconds: Some(10),
                })
                .collect(),
            forecast: ForecastConfig::default(),
        };

        struct ReleaseFile<'a>(&'a std::path::Path);
        impl Drop for ReleaseFile<'_> {
            fn drop(&mut self) {
                let _ = std::fs::write(self.0, b"go");
            }
        }
        let _release = ReleaseFile(&release);

        let worker = thread::spawn(move || refresh(&config));
        let observe_until = Instant::now() + StdDuration::from_secs(3);
        let mut max_seen = 0usize;
        let mut saw_cap = false;
        while Instant::now() < observe_until {
            let count = std::fs::read_dir(&started).unwrap().count();
            max_seen = max_seen.max(count);
            if count >= 2 {
                saw_cap = true;
                thread::sleep(StdDuration::from_millis(250));
                max_seen = max_seen.max(std::fs::read_dir(&started).unwrap().count());
                break;
            }
            thread::sleep(StdDuration::from_millis(20));
        }
        std::fs::write(&release, b"go").unwrap();
        let state = worker.join().expect("refresh thread");
        assert!(saw_cap, "expected two command sources to run together");
        assert!(
            max_seen <= 2,
            "max_concurrent_refreshes=2 allowed {max_seen} in flight"
        );
        assert_eq!(state.providers.len(), 4);
        assert!(state.validate().is_ok());
    }

    #[test]
    fn deadline_without_last_good_marks_every_slow_source() {
        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 30,
            max_concurrent_refreshes: 2,
            refresh_deadline_seconds: 1,
            sources: vec![
                Source::Command {
                    program: "sleep".into(),
                    args: vec!["30".into()],
                    timeout_seconds: Some(30),
                },
                Source::Command {
                    program: "sleep".into(),
                    args: vec!["30".into()],
                    timeout_seconds: Some(30),
                },
            ],
            forecast: ForecastConfig::default(),
        };
        let started = Instant::now();
        let state = refresh(&config);
        assert!(
            started.elapsed() < StdDuration::from_secs(3),
            "wave ran for {:?}",
            started.elapsed()
        );
        assert_eq!(state.providers.len(), 2);
        assert_ne!(state.providers[0].id, state.providers[1].id);
        for provider in &state.providers {
            assert_eq!(provider.status, "unavailable");
            assert_eq!(provider.detail.as_deref(), Some(GLOBAL_DEADLINE_DETAIL));
            assert!(provider.updated_at.is_none());
        }
        assert!(state.validate().is_ok());
    }

    #[test]
    fn forecast_history_updates_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let history_file = dir.path().join("history.json");
        let forecast_cfg = ForecastConfig {
            enabled: true,
            max_observations_per_window: 10,
            stale_after_seconds: 600,
            notify_thresholds: vec![],
            history_path: Some(history_file.clone()),
            notifications_path: None,
        };
        let state = AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: vec![fresh_provider("fast-provider", 80.0)],
        };
        update_forecast_history(&forecast_cfg, &state);
        let loaded = forecast::load_history(&history_file).unwrap();
        assert_eq!(loaded.windows.len(), 1);
        assert_eq!(loaded.windows[0].provider, "fast-provider");
        assert_eq!(loaded.windows[0].observations.len(), 1);
        assert_eq!(loaded.windows[0].observations[0].remaining_percent, 80.0);
    }
}
