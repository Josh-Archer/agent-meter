use agent_meter::forecast::NotifySink;
use agent_meter::usage::{self, UsageStore};
use agent_meter::{
    forecast, runtime_state_path, write_state, AgentMeterState, ProviderState, UsageWindow,
    STATE_VERSION,
};
use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use clap::Parser;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
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
    /// Optional usage ingest listener, for example `100.64.0.1:8787`.
    /// Off by default. Refuses `0.0.0.0` and `::` unless `--listen-any` is set.
    #[arg(long)]
    listen: Option<String>,
    /// Allow `--listen` to bind an unspecified address (`0.0.0.0` or `::`).
    #[arg(long)]
    listen_any: bool,
    /// SHA-256 host token file. Defaults to
    /// `$XDG_CONFIG_HOME/agent-meter/ingest-tokens.json`.
    /// Values are hashes, never bearer tokens.
    #[arg(long)]
    ingest_tokens: Option<PathBuf>,
    /// Refresh only the matching provider (used with `--once`).
    #[arg(long)]
    provider: Option<String>,
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
            reason: None,
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
        remaining_percent: Some(remaining),
        resets_at: Some(reset),
        reset_label: Some(reset_label.into()),
    }
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agent-meter/sources.json")
}

fn default_ingest_tokens_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agent-meter/ingest-tokens.json")
}

fn usage_root() -> PathBuf {
    agent_meter::stable_state_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("agent-meter"))
}

fn open_usage_store() -> Result<Arc<Mutex<UsageStore>>> {
    let root = usage_root();
    let mut store = UsageStore::open(usage::UsagePaths::from_root(&root))?;
    if let Some(parent) = runtime_state_path().parent() {
        store.set_extra_today_paths(vec![parent.join(usage::TODAY_FILE_NAME)]);
    }
    Ok(Arc::new(Mutex::new(store)))
}

fn drain_usage(store: &Arc<Mutex<UsageStore>>) {
    let mut guard = store.lock().unwrap_or_else(|err| err.into_inner());
    match guard.drain_inbox() {
        Ok(stats) if stats.seen > 0 => {
            eprintln!(
                "agent-meterd: inbox accepted={} duplicates={} rejected={}",
                stats.accepted, stats.duplicates, stats.rejected
            );
        }
        Ok(_) => {}
        Err(err) => eprintln!("agent-meterd: inbox drain failed: {err}"),
    }
    if let Err(err) = guard.publish_today_now() {
        eprintln!("agent-meterd: could not write usage today: {err}");
    }
}

fn warn_token_file_mode(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.permissions().mode() & 0o077 != 0 {
                eprintln!(
                    "agent-meterd: {} is readable by group or others; mode 0600 is safer for token hashes",
                    path.display()
                );
            }
        }
    }
}

/// Watch the inbox for atomic renames. `None` means the refresh loop drains it.
fn arm_inbox_watch(dir: &Path) -> Option<i32> {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        eprintln!("agent-meterd: inotify unavailable; inbox drains on the refresh interval");
        return None;
    }
    let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
        unsafe { libc::close(fd) };
        return None;
    };
    let mask = libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE;
    let watch = unsafe { libc::inotify_add_watch(fd, path.as_ptr(), mask) };
    if watch < 0 {
        eprintln!("agent-meterd: inotify watch failed; inbox drains on the refresh interval");
        unsafe { libc::close(fd) };
        return None;
    }
    Some(fd)
}

fn wait_inbox(fd: Option<i32>, timeout: StdDuration) {
    let Some(fd) = fd else {
        thread::sleep(timeout);
        return;
    };
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
    if ready <= 0 {
        return;
    }
    let mut buf = [0u8; 4096];
    loop {
        let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
        if read <= 0 {
            break;
        }
    }
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
    NotFound,
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
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                CommandError::NotFound
            } else {
                CommandError::Unavailable
            }
        })?;

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

const REASON_CLI_NOT_INSTALLED: &str = "cli_not_installed";
const REASON_SOURCE_FAILED: &str = "source_failed";
const REASON_TIMEOUT: &str = "timeout";

fn sanitize_state(state: &mut AgentMeterState) {
    state.version = STATE_VERSION;
    for (index, provider) in state.providers.iter_mut().enumerate() {
        if let Err(err) = provider.validate() {
            let hint = if !provider.id.is_empty() {
                provider.id.clone()
            } else {
                format!("provider-{index}")
            };
            *provider = unavailable(
                &hint,
                &format!("Invalid provider document: {err}"),
                REASON_SOURCE_FAILED,
            );
        }
    }
    deduplicate_provider_ids(&mut state.providers);
}

enum SourceRefresh {
    /// A normalized document from the source, which may itself be unavailable.
    Document(ProviderState),
    /// The source produced no document. `detail` is the failure message and
    /// `reason` is the machine-readable cause.
    Failed {
        hint: String,
        detail: String,
        reason: &'static str,
    },
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

fn normalize_provider_key(raw: &str) -> String {
    let normalized: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = normalized.trim_matches('-');
    if trimmed == "agy" {
        "antigravity".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn source_matches_provider(
    index: usize,
    source: &Source,
    seeded: Option<&ProviderState>,
    target: &str,
) -> bool {
    let target_key = normalize_provider_key(target);
    if target_key.is_empty() {
        return false;
    }
    if normalize_provider_key(&source_hint(index, source)) == target_key {
        return true;
    }
    if let Some(provider) = seeded {
        if normalize_provider_key(&provider.id) == target_key {
            return true;
        }
    }
    match source {
        Source::Mock { provider } => normalize_provider_key(&provider.id) == target_key,
        Source::File { path } => path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|stem| normalize_provider_key(stem) == target_key),
        Source::Command { program, args, .. } => {
            let program_path = Path::new(program);
            if program_path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| normalize_provider_key(stem) == target_key)
            {
                return true;
            }
            if program_path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|name| normalize_provider_key(name) == target_key)
            {
                return true;
            }
            args.iter().any(|arg| {
                !arg.starts_with('-')
                    && !arg.is_empty()
                    && arg
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    && normalize_provider_key(arg) == target_key
            })
        }
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
                reason: REASON_SOURCE_FAILED,
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
            reason: REASON_TIMEOUT,
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
                        reason: REASON_SOURCE_FAILED,
                    },
                },
                Err(_) => SourceRefresh::Failed {
                    hint: fallback_hint,
                    detail: "File adapter could not read normalized state".into(),
                    reason: REASON_SOURCE_FAILED,
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
                    reason: REASON_TIMEOUT,
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
                            reason: REASON_SOURCE_FAILED,
                        },
                    }
                }
                Ok(_) => SourceRefresh::Failed {
                    hint: program.clone(),
                    detail: "Adapter exited unsuccessfully".into(),
                    reason: REASON_SOURCE_FAILED,
                },
                Err(CommandError::Timeout) if capped_by_deadline => SourceRefresh::Failed {
                    hint: program.clone(),
                    detail: GLOBAL_DEADLINE_DETAIL.into(),
                    reason: REASON_TIMEOUT,
                },
                Err(CommandError::Timeout) => SourceRefresh::Failed {
                    hint: program.clone(),
                    detail: "Adapter timed out".into(),
                    reason: REASON_TIMEOUT,
                },
                Err(CommandError::NotFound) => SourceRefresh::Failed {
                    hint: program.clone(),
                    detail: "Adapter executable is unavailable".into(),
                    reason: REASON_CLI_NOT_INSTALLED,
                },
                Err(CommandError::Unavailable) => SourceRefresh::Failed {
                    hint: program.clone(),
                    detail: "Adapter executable is unavailable".into(),
                    reason: REASON_SOURCE_FAILED,
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
        SourceRefresh::Failed {
            hint,
            detail,
            reason,
        } => {
            if let Some(previous) = last_good.get(&index).filter(|p| keeps_last_good(p)) {
                let stale = mark_stale(previous.clone(), &detail);
                last_good.insert(index, stale.clone());
                stale
            } else {
                last_good.remove(&index);
                unavailable(&hint, &detail, reason)
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

#[cfg(test)]
fn refresh_publishing(
    config: &Config,
    last_good: &mut HashMap<usize, ProviderState>,
    publish: impl FnMut(&AgentMeterState),
) -> AgentMeterState {
    refresh_publishing_target(config, None, last_good, publish)
}

/// Refresh either all sources (`target_provider = None`) or only sources
/// matching `target_provider`, preserving existing `last_good` entries in
/// `slots` for non-targeted sources without executing their adapter commands.
///
/// `publish` runs as soon as a source settles, with source order preserved.
/// Providers still running keep their previous last-good snapshot. The wave
/// stops at `refresh_deadline_seconds` even when a per-source timeout is longer.
/// `last_good` survives across calls so a later failure can be marked stale
/// without moving that provider's `updated_at`.
fn refresh_publishing_target(
    config: &Config,
    target_provider: Option<&str>,
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

    let targeted: Vec<bool> = match target_provider {
        Some(target) => config
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                source_matches_provider(index, source, last_good.get(&index), target)
            })
            .collect(),
        None => vec![true; config.sources.len()],
    };

    let mut slots = vec![None; config.sources.len()];
    let mut targeted_indices = Vec::new();
    for (index, &is_targeted) in targeted.iter().enumerate() {
        if is_targeted {
            targeted_indices.push(index);
        } else if let Some(existing) = last_good.get(&index) {
            slots[index] = Some(existing.clone());
        }
    }

    if targeted_indices.is_empty() {
        let latest = snapshot_from(&slots, last_good, false);
        publish(&latest);
        return latest;
    }

    let deadline = refresh_deadline_instant(config);
    let worker_count = effective_concurrency(config)
        .min(targeted_indices.len())
        .max(1);
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

        let mut outstanding = targeted_indices.len();
        for index in targeted_indices {
            let _ = job_tx.send(index);
        }
        drop(job_tx);

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

        if (0..slots.len()).any(|index| targeted[index] && slots[index].is_none()) {
            for index in 0..slots.len() {
                if targeted[index] && slots[index].is_none() {
                    record_source(
                        index,
                        SourceRefresh::Failed {
                            hint: source_hint(index, &config.sources[index]),
                            detail: GLOBAL_DEADLINE_DETAIL.into(),
                            reason: REASON_TIMEOUT,
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

fn normalize_id(hint: &str) -> String {
    let normalized: String = hint
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let id = normalized.trim_matches('-');
    if id.is_empty() {
        "adapter".to_owned()
    } else {
        id.to_owned()
    }
}

fn canonical_provider_id(id: &str) -> &str {
    match id {
        "agy" => "antigravity",
        other => other,
    }
}

fn is_seedable_provider(provider: &ProviderState) -> bool {
    keeps_last_good(provider)
        || provider
            .windows
            .iter()
            .any(|window| window.remaining_percent.is_some())
}

fn source_secondary_hints(source: &Source) -> Vec<String> {
    let mut hints = Vec::new();
    match source {
        Source::Mock { .. } => {}
        Source::File { path } => {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                hints.push(normalize_id(stem));
            }
        }
        Source::Command { program, args, .. } => {
            if let Some(stem) = Path::new(program).file_stem().and_then(|s| s.to_str()) {
                hints.push(normalize_id(stem));
            }
            for arg in args {
                if !arg.starts_with('-') && !arg.contains(char::is_whitespace) && !arg.contains('{')
                {
                    hints.push(normalize_id(arg));
                }
            }
        }
    }
    hints
}

#[derive(Copy, Clone)]
enum MatchTier {
    PrimaryExact,
    PrimaryAlias,
    SecondaryExact,
    SecondaryAlias,
}

fn source_matches_tier(
    primary: &str,
    secondary: &[String],
    provider_id: &str,
    tier: MatchTier,
) -> bool {
    match tier {
        MatchTier::PrimaryExact => primary == provider_id,
        MatchTier::PrimaryAlias => {
            canonical_provider_id(primary) == canonical_provider_id(provider_id)
        }
        MatchTier::SecondaryExact => secondary.iter().any(|hint| hint == provider_id),
        MatchTier::SecondaryAlias => {
            let canonical_provider = canonical_provider_id(provider_id);
            secondary
                .iter()
                .any(|hint| canonical_provider_id(hint) == canonical_provider)
        }
    }
}

/// Seed `last_good` from a previously persisted `AgentMeterState` so slow
/// sources retain their last known good snapshot while their startup refresh
/// is still in flight.
fn seed_last_good_from_state(
    config: &Config,
    existing: &AgentMeterState,
) -> HashMap<usize, ProviderState> {
    let mut seeded = HashMap::new();
    if config.sources.is_empty() || existing.providers.is_empty() {
        return seeded;
    }

    let source_hints: Vec<(String, Vec<String>)> = config
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            (
                normalize_id(&source_hint(index, source)),
                source_secondary_hints(source),
            )
        })
        .collect();
    let eligible: Vec<(usize, &ProviderState)> = existing
        .providers
        .iter()
        .enumerate()
        .filter(|(_, provider)| is_seedable_provider(provider))
        .collect();
    let mut assigned_sources = vec![false; config.sources.len()];
    let mut matched_providers = vec![false; existing.providers.len()];

    for tier in [
        MatchTier::PrimaryExact,
        MatchTier::PrimaryAlias,
        MatchTier::SecondaryExact,
        MatchTier::SecondaryAlias,
    ] {
        for &(existing_idx, provider) in &eligible {
            if matched_providers[existing_idx] {
                continue;
            }
            let provider_id = normalize_id(&provider.id);
            let preferred = (existing_idx < config.sources.len()
                && !assigned_sources[existing_idx]
                && source_matches_tier(
                    &source_hints[existing_idx].0,
                    &source_hints[existing_idx].1,
                    &provider_id,
                    tier,
                ))
            .then_some(existing_idx);
            let target_idx = preferred.or_else(|| {
                source_hints
                    .iter()
                    .enumerate()
                    .find_map(|(source_idx, (primary, secondary))| {
                        (!assigned_sources[source_idx]
                            && source_matches_tier(primary, secondary, &provider_id, tier))
                        .then_some(source_idx)
                    })
            });
            if let Some(source_idx) = target_idx {
                assigned_sources[source_idx] = true;
                matched_providers[existing_idx] = true;
                seeded.insert(source_idx, provider.clone());
            }
        }
    }

    seeded
}

fn state_from_json_output(output: &agent_meter::JsonOutput) -> Option<AgentMeterState> {
    if output.schema_version != 1 {
        return None;
    }
    let mut providers = Vec::with_capacity(output.providers.len());
    let mut latest_updated: Option<chrono::DateTime<Utc>> = None;
    for jp in &output.providers {
        let updated_at = chrono::DateTime::parse_from_rfc3339(&jp.updated_at)
            .ok()
            .map(|dt| dt.with_timezone(&Utc));
        if let Some(ts) = updated_at {
            latest_updated = Some(latest_updated.map_or(ts, |cur| cur.max(ts)));
        }
        let status = if !jp.available {
            "unavailable"
        } else if jp.stale {
            "stale"
        } else {
            "fresh"
        };
        let windows = jp
            .windows
            .iter()
            .map(|w| UsageWindow {
                id: w.name.clone(),
                label: w.name.clone(),
                remaining_percent: w.remaining_percent,
                resets_at: w
                    .resets_at
                    .as_deref()
                    .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                    .map(|dt| dt.with_timezone(&Utc)),
                reset_label: None,
            })
            .collect();
        providers.push(ProviderState {
            id: jp.provider.clone(),
            label: jp.provider.clone(),
            icon: jp.provider.clone(),
            windows,
            status: status.into(),
            detail: None,
            reason: jp.reason.clone(),
            usage_url: None,
            updated_at,
        });
    }
    let state = AgentMeterState {
        version: STATE_VERSION,
        generated_at: latest_updated.unwrap_or_else(Utc::now),
        providers,
    };
    state.validate().ok()?;
    Some(state)
}

fn load_state_file(path: &Path, allow_json_output: bool) -> Option<AgentMeterState> {
    if let Ok(state) = agent_meter::read_state(path) {
        return Some(state);
    }
    if allow_json_output {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(output) = serde_json::from_str::<agent_meter::JsonOutput>(&contents) {
                return state_from_json_output(&output);
            }
        }
    }
    None
}

fn load_existing_state_with_fallback(
    state_path: &Path,
    fallback_path: &Path,
) -> Option<AgentMeterState> {
    if let Some(state) = load_state_file(state_path, false) {
        return Some(state);
    }
    if fallback_path != state_path {
        return load_state_file(fallback_path, true);
    }
    None
}

fn load_existing_state(state_path: &std::path::Path) -> Option<AgentMeterState> {
    load_existing_state_with_fallback(state_path, &agent_meter::stable_state_path())
}

fn unavailable(id_hint: &str, detail: &str, reason: &str) -> ProviderState {
    let id = normalize_id(id_hint);
    ProviderState {
        id,
        label: id_hint.into(),
        icon: "generic".into(),
        windows: vec![UsageWindow {
            id: "availability".into(),
            label: "Availability".into(),
            remaining_percent: None,
            resets_at: None,
            reset_label: Some("unavailable".into()),
        }],
        status: "unavailable".into(),
        detail: Some(detail.into()),
        usage_url: None,
        reason: Some(reason.into()),
        updated_at: None,
    }
}

fn write_stable_json(state: &AgentMeterState) {
    if let Err(e) = agent_meter::write_stable_state(state) {
        eprintln!("agent-meterd: could not write stable JSON: {e}");
    }
}

fn forecast_history_path(config: &ForecastConfig) -> PathBuf {
    config
        .history_path
        .clone()
        .unwrap_or_else(forecast::default_history_path)
}

fn load_forecast_history(config: &ForecastConfig) -> forecast::UsageHistory {
    let history_path = forecast_history_path(config);
    forecast::load_history(&history_path).unwrap_or_else(|err| {
        eprintln!("agent-meterd: could not load usage history: {err}");
        forecast::UsageHistory {
            version: forecast::HISTORY_VERSION,
            windows: Vec::new(),
        }
    })
}

/// Record the refresh in the bounded usage history, emit any configured
/// threshold notifications, and flush the updated history once to disk.
fn update_forecast_history(
    config: &ForecastConfig,
    history: &mut forecast::UsageHistory,
    state: &AgentMeterState,
) {
    update_forecast_history_with(config, history, state, forecast::save_history);
}

fn update_forecast_history_with<F>(
    config: &ForecastConfig,
    history: &mut forecast::UsageHistory,
    state: &AgentMeterState,
    mut save: F,
) where
    F: FnMut(&Path, &forecast::UsageHistory) -> Result<()>,
{
    let history_path = forecast_history_path(config);
    let now = Utc::now();
    forecast::record_state(history, state, now, config.max_observations_per_window);
    if !config.notify_thresholds.is_empty() {
        let events = forecast::check_notifications(
            history,
            &config.notify_thresholds,
            now,
            config.stale_after_seconds,
        );
        if !events.is_empty() {
            let sink_path = config
                .notifications_path
                .clone()
                .unwrap_or_else(forecast::default_notifications_path);
            let mut sink = forecast::FileLogSink::new(sink_path);
            for event in &events {
                sink.notify(event);
            }
        }
    }
    if let Err(err) = save(&history_path, history) {
        eprintln!("agent-meterd: could not write usage history: {err}");
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
    if args.listen_any && args.listen.is_none() {
        anyhow::bail!("--listen-any requires --listen");
    }
    let config_path = args.config.unwrap_or_else(config_path);
    let state_path = args.state.unwrap_or_else(runtime_state_path);
    let config = load_config(&config_path)?;
    if config.refresh_seconds == 0 {
        anyhow::bail!("refresh_seconds must be greater than zero");
    }

    // Usage ingest is local-only unless --listen is set. A missing store does
    // not block quota refresh; the listener cannot start without one.
    let usage_store = match open_usage_store() {
        Ok(store) => Some(store),
        Err(err) if args.listen.is_some() => return Err(err),
        Err(err) => {
            eprintln!("agent-meterd: usage store unavailable: {err}");
            None
        }
    };
    let listener = if let Some(spec) = args.listen.as_deref() {
        let store = usage_store
            .clone()
            .context("usage store is required for --listen")?;
        let token_path = args
            .ingest_tokens
            .clone()
            .unwrap_or_else(default_ingest_tokens_path);
        warn_token_file_mode(&token_path);
        let listener = usage::serve(spec, args.listen_any, &token_path, store)?;
        eprintln!("agent-meterd: usage ingest listening on {}", listener.addr);
        Some(listener)
    } else {
        None
    };

    let inbox = usage_root().join(usage::INBOX_DIR_NAME);
    if usage_store.is_some() {
        let _ = std::fs::create_dir_all(&inbox);
    }
    let inbox_watch = if usage_store.is_some() && (args.watch || listener.is_some()) {
        arm_inbox_watch(&inbox)
    } else {
        None
    };

    let mut forecast_history = if config.forecast.enabled {
        Some(load_forecast_history(&config.forecast))
    } else {
        None
    };

    let existing_state = load_existing_state(&state_path);
    let mut last_good: HashMap<usize, ProviderState> = existing_state
        .as_ref()
        .map(|existing| seed_last_good_from_state(&config, existing))
        .unwrap_or_default();
    let mut last_good_state: Option<AgentMeterState> = existing_state;
    let mut did_refresh = false;
    let mut next_refresh = Instant::now();
    loop {
        if let Some(store) = &usage_store {
            drain_usage(store);
        }
        if (args.watch || !did_refresh) && Instant::now() >= next_refresh {
            let mut write_error: Option<anyhow::Error> = None;
            let mut wrote_this_refresh = false;
            let final_state =
                refresh_publishing_target(&config, args.provider.as_deref(), &mut last_good, |state| {
                    match emit_state(&state_path, state, &mut last_good_state) {
                        Ok(()) => wrote_this_refresh = true,
                        Err(err) => write_error = Some(err),
                    }
                });
            if let Some(history) = forecast_history.as_mut() {
                update_forecast_history(&config.forecast, history, &final_state);
            }
            if !wrote_this_refresh {
                if let Some(err) = write_error {
                    if !args.watch {
                        return Err(err);
                    }
                }
            }
            did_refresh = true;
            next_refresh = Instant::now() + StdDuration::from_secs(config.refresh_seconds);
        }
        if !args.watch && listener.is_none() {
            break;
        }
        let timeout = if args.watch {
            next_refresh.saturating_duration_since(Instant::now())
        } else {
            StdDuration::from_secs(60)
        };
        wait_inbox(inbox_watch, timeout);
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
    fn command_not_found_on_missing_executable() {
        let result = run_command_with_timeout(
            "/nonexistent/binary/for/test",
            &[],
            StdDuration::from_secs(1),
        );
        assert!(matches!(result, Err(CommandError::NotFound)));
    }

    #[test]
    fn command_unavailable_on_non_executable_target() {
        let dir = tempfile::tempdir().unwrap();
        let result =
            run_command_with_timeout(dir.path().to_str().unwrap(), &[], StdDuration::from_secs(1));
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
                remaining_percent: Some(50.0),
                resets_at: None,
                reset_label: None,
            }],
            status: "fresh".into(),
            detail: None,
            usage_url: None,
            reason: None,
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
                remaining_percent: Some(75.0),
                resets_at: None,
                reset_label: None,
            }],
            status: "fresh".into(),
            detail: None,
            usage_url: None,
            reason: None,
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
        assert_eq!(good.windows[0].remaining_percent, Some(75.0));

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
                    reason: None,
                    updated_at: None,
                },
                ProviderState {
                    id: "duplicate".into(),
                    label: "Second".into(),
                    icon: "icon".into(),
                    windows: vec![UsageWindow {
                        id: "win".into(),
                        label: "Win".into(),
                        remaining_percent: Some(50.0),
                        resets_at: None,
                        reset_label: None,
                    }],
                    status: "fresh".into(),
                    detail: None,
                    usage_url: None,
                    reason: None,
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
                remaining_percent: Some(percent),
                resets_at: None,
                reset_label: None,
            }],
            status: "fresh".into(),
            detail: None,
            usage_url: None,
            reason: None,
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
        assert_eq!(codex.windows[0].remaining_percent, Some(64.0));
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
        assert_eq!(second.providers[0].windows[0].remaining_percent, Some(70.0));

        let third = refresh_publishing(&failure, &mut last_good, |_| {});
        assert_eq!(third.providers[0].status, "stale");
        assert_eq!(third.providers[0].updated_at, Some(stamp));
        assert_eq!(third.providers[0].windows[0].remaining_percent, Some(70.0));
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
    fn listen_is_off_unless_requested() {
        let args = Args::try_parse_from(["agent-meterd", "--once"]).unwrap();
        assert!(args.listen.is_none());
        assert!(!args.listen_any);
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
        let mut history = load_forecast_history(&forecast_cfg);
        update_forecast_history(&forecast_cfg, &mut history, &state);
        assert_eq!(history.windows.len(), 1);
        assert_eq!(history.windows[0].observations.len(), 1);
        let loaded = forecast::load_history(&history_file).unwrap();
        assert_eq!(loaded.windows.len(), 1);
        assert_eq!(loaded.windows[0].provider, "fast-provider");
        assert_eq!(loaded.windows[0].observations.len(), 1);
        assert_eq!(loaded.windows[0].observations[0].remaining_percent, 80.0);
    }

    #[test]
    fn unavailable_omits_remaining_percent_and_sets_reason() {
        let provider = unavailable("codex", "codex unavailable", REASON_CLI_NOT_INSTALLED);
        assert_eq!(provider.status, "unavailable");
        assert_eq!(provider.reason.as_deref(), Some(REASON_CLI_NOT_INSTALLED));
        assert_eq!(provider.windows.len(), 1);
        assert_eq!(provider.windows[0].remaining_percent, None);

        let json_output = agent_meter::JsonOutput::from_state(&AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: vec![provider],
        });
        let value = serde_json::to_value(&json_output).unwrap();
        let p = &value["providers"][0];
        assert_eq!(p["available"], false);
        assert_eq!(p["stale"], false);
        assert_eq!(p["reason"], REASON_CLI_NOT_INSTALLED);
        assert!(p["windows"][0].get("remaining_percent").is_none());
    }

    #[test]
    fn refresh_sets_specific_reasons_and_preserves_real_zero() {
        let dir = tempfile::tempdir().unwrap();
        let missing_file = dir.path().join("missing.json");
        let real_zero = fresh_provider("real-zero", 0.0);
        let zero_echo = echo_provider(&real_zero);

        let cfg = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 4,
            refresh_deadline_seconds: 5,
            sources: vec![
                Source::Command {
                    program: "/nonexistent/cli/for/test".into(),
                    args: vec![],
                    timeout_seconds: None,
                },
                Source::Command {
                    program: "false".into(),
                    args: vec![],
                    timeout_seconds: None,
                },
                Source::Command {
                    program: "sleep".into(),
                    args: vec!["2".into()],
                    timeout_seconds: Some(1),
                },
                Source::File { path: missing_file },
                zero_echo,
            ],
            forecast: ForecastConfig::default(),
        };

        let state = refresh(&cfg);
        assert_eq!(state.providers.len(), 5);

        let missing_cli = &state.providers[0];
        assert_eq!(missing_cli.status, "unavailable");
        assert_eq!(
            missing_cli.reason.as_deref(),
            Some(REASON_CLI_NOT_INSTALLED)
        );
        assert_eq!(missing_cli.windows[0].remaining_percent, None);

        let failing_cmd = &state.providers[1];
        assert_eq!(failing_cmd.status, "unavailable");
        assert_eq!(failing_cmd.reason.as_deref(), Some(REASON_SOURCE_FAILED));
        assert_eq!(failing_cmd.windows[0].remaining_percent, None);

        let slow_cmd = &state.providers[2];
        assert_eq!(slow_cmd.status, "unavailable");
        assert_eq!(slow_cmd.reason.as_deref(), Some(REASON_TIMEOUT));
        assert_eq!(slow_cmd.windows[0].remaining_percent, None);

        let missing_file_p = &state.providers[3];
        assert_eq!(missing_file_p.status, "unavailable");
        assert_eq!(missing_file_p.reason.as_deref(), Some(REASON_SOURCE_FAILED));
        assert_eq!(missing_file_p.windows[0].remaining_percent, None);

        let real_zero_p = &state.providers[4];
        assert_eq!(real_zero_p.status, "fresh");
        assert_eq!(real_zero_p.reason, None);
        assert_eq!(real_zero_p.windows[0].remaining_percent, Some(0.0));

        let json_output = agent_meter::JsonOutput::from_state(&state);
        let value = serde_json::to_value(&json_output).unwrap();
        assert_eq!(
            value["providers"][4]["windows"][0]["remaining_percent"],
            0.0
        );
        assert!(value["providers"][4].get("reason").is_none());
    }

    #[test]
    fn forecast_history_ignores_unknown_and_fires_no_notifications() {
        let dir = tempfile::tempdir().unwrap();
        let history_file = dir.path().join("history.json");
        let notifications_file = dir.path().join("notifications.json");
        let forecast_cfg = ForecastConfig {
            enabled: true,
            max_observations_per_window: 10,
            stale_after_seconds: 600,
            notify_thresholds: vec![100.0, 25.0, 0.0],
            history_path: Some(history_file.clone()),
            notifications_path: Some(notifications_file.clone()),
        };
        let state = AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: vec![
                unavailable("codex", "missing codex", REASON_CLI_NOT_INSTALLED),
                unavailable("antigravity", "failed agy", REASON_SOURCE_FAILED),
            ],
        };
        let mut history = load_forecast_history(&forecast_cfg);
        update_forecast_history(&forecast_cfg, &mut history, &state);
        assert!(history.windows.is_empty());
        let loaded = forecast::load_history(&history_file).unwrap();
        assert!(loaded.windows.is_empty());
        assert!(!notifications_file.exists());
    }

    #[test]
    fn seed_last_good_matches_sources_and_agy_alias() {
        let mut codex = fresh_provider("codex", 68.0);
        codex.updated_at = Some(Utc::now() - Duration::minutes(5));
        let mut antigravity = fresh_provider("antigravity", 91.0);
        antigravity.updated_at = Some(Utc::now() - Duration::minutes(4));
        let mut claude = fresh_provider("claude", 42.0);
        claude.status = "error".into();
        let placeholder_unavailable = unavailable("grok", "missing cli", REASON_CLI_NOT_INSTALLED);
        let mut error_without_windows = unavailable("muse", "errored", REASON_SOURCE_FAILED);
        error_without_windows.status = "error".into();

        let existing = AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: vec![
                antigravity.clone(),
                codex.clone(),
                placeholder_unavailable,
                error_without_windows,
                claude.clone(),
            ],
        };

        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 4,
            refresh_deadline_seconds: 5,
            sources: vec![
                Source::Command {
                    program: "codex".into(),
                    args: vec![],
                    timeout_seconds: None,
                },
                Source::Command {
                    program: "/usr/local/bin/agent-meter-adapter".into(),
                    args: vec!["agy".into()],
                    timeout_seconds: None,
                },
                Source::Command {
                    program: "grok".into(),
                    args: vec![],
                    timeout_seconds: None,
                },
                Source::Command {
                    program: "muse".into(),
                    args: vec![],
                    timeout_seconds: None,
                },
                Source::File {
                    path: PathBuf::from("/tmp/state/claude.json"),
                },
            ],
            forecast: ForecastConfig::default(),
        };

        let seeded = seed_last_good_from_state(&config, &existing);
        assert_eq!(seeded.len(), 3);
        assert_eq!(seeded.get(&0), Some(&codex));
        assert_eq!(seeded.get(&1), Some(&antigravity));
        assert_eq!(seeded.get(&4), Some(&claude));
        assert!(!seeded.contains_key(&2));
        assert!(!seeded.contains_key(&3));

        // Direct "agy" command hint and unassigned index fallback with duplicate source IDs.
        let direct_config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 4,
            refresh_deadline_seconds: 5,
            sources: vec![
                Source::Command {
                    program: "agy".into(),
                    args: vec![],
                    timeout_seconds: None,
                },
                Source::Mock {
                    provider: fresh_provider("codex", 10.0),
                },
                Source::Mock {
                    provider: fresh_provider("codex", 20.0),
                },
            ],
            forecast: ForecastConfig::default(),
        };
        let dup_existing = AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: vec![codex.clone(), codex.clone(), antigravity.clone()],
        };
        let dup_seeded = seed_last_good_from_state(&direct_config, &dup_existing);
        assert_eq!(dup_seeded.get(&0), Some(&antigravity));
        assert_eq!(dup_seeded.get(&1), Some(&codex));
        assert_eq!(dup_seeded.get(&2), Some(&codex));
    }

    #[test]
    fn seed_last_good_preserves_seeded_entries_during_refresh_publishing() {
        let prev_stamp = Utc::now() - Duration::minutes(10);
        let mut prev_agy = fresh_provider("antigravity", 77.0);
        prev_agy.updated_at = Some(prev_stamp);

        let existing = AgentMeterState {
            version: STATE_VERSION,
            generated_at: prev_stamp,
            providers: vec![prev_agy.clone()],
        };
        let fast = fresh_provider("fast-provider", 55.0);
        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 30,
            max_concurrent_refreshes: 2,
            refresh_deadline_seconds: 1,
            sources: vec![
                Source::Command {
                    program: "agy".into(),
                    args: vec![],
                    timeout_seconds: Some(30),
                },
                echo_provider(&fast),
            ],
            forecast: ForecastConfig::default(),
        };
        let mut seeded = seed_last_good_from_state(&config, &existing);
        assert_eq!(seeded.get(&0), Some(&prev_agy));

        // Replace source 0 with a hung command while keeping the seeded last_good
        // at index 0, simulating a slow startup poll that misses the deadline.
        let slow_config = Config {
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
                echo_provider(&fast),
            ],
            forecast: ForecastConfig::default(),
        };

        let mut snapshots = Vec::new();
        let final_state = refresh_publishing(&slow_config, &mut seeded, |snapshot| {
            snapshots.push(snapshot.clone());
        });

        // While source 0 is still in flight and source 1 publishes, the seeded
        // antigravity snapshot must remain present rather than disappearing.
        let inflight_snapshot = snapshots
            .iter()
            .find(|snap| {
                snap.providers
                    .iter()
                    .any(|p| p.id == "fast-provider" && p.status == "fresh")
            })
            .expect("fast-provider should publish first");
        let inflight_agy = inflight_snapshot
            .providers
            .iter()
            .find(|p| p.id == "antigravity")
            .expect("seeded antigravity should remain present while in flight");
        assert_eq!(inflight_agy.status, "fresh");
        assert_eq!(inflight_agy.windows[0].remaining_percent, Some(77.0));
        assert_eq!(inflight_agy.updated_at, Some(prev_stamp));

        // After the deadline expires, antigravity becomes stale while retaining
        // its prior windows and updated_at instead of dropping to unavailable.
        let settled_agy = final_state
            .providers
            .iter()
            .find(|p| p.id == "antigravity")
            .expect("settled antigravity should stay in final state");
        assert_eq!(settled_agy.status, "stale");
        assert_eq!(settled_agy.windows[0].remaining_percent, Some(77.0));
        assert_eq!(settled_agy.updated_at, Some(prev_stamp));
    }

    #[test]
    fn load_existing_state_reads_primary_and_falls_back_to_stable_state() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime-state.json");
        let stable = dir.path().join("stable-state.json");

        let mut codex = fresh_provider("codex", 83.0);
        let stamp = Utc::now() - Duration::minutes(3);
        codex.updated_at = Some(stamp);
        let primary_state = AgentMeterState {
            version: STATE_VERSION,
            generated_at: stamp,
            providers: vec![codex.clone()],
        };
        agent_meter::write_state(&runtime, &primary_state).unwrap();

        let loaded = load_existing_state_with_fallback(&runtime, &stable).unwrap();
        assert_eq!(loaded, primary_state);

        // Missing or unreadable runtime path falls back to stable JsonOutput.
        std::fs::write(&runtime, "{not-valid-json").unwrap();
        agent_meter::write_json_output(&stable, &primary_state).unwrap();
        let fallback_loaded = load_existing_state_with_fallback(&runtime, &stable).unwrap();
        assert_eq!(fallback_loaded.providers.len(), 1);
        assert_eq!(fallback_loaded.providers[0].id, "codex");
        assert_eq!(fallback_loaded.providers[0].status, "fresh");
        assert_eq!(
            fallback_loaded.providers[0].windows[0].remaining_percent,
            Some(83.0)
        );
        assert_eq!(
            fallback_loaded.providers[0].updated_at.unwrap().timestamp(),
            stamp.timestamp()
        );

        // Fallback also reads AgentMeterState directly when present.
        let missing = dir.path().join("does-not-exist.json");
        agent_meter::write_state(&stable, &primary_state).unwrap();
        let fallback_state = load_existing_state_with_fallback(&missing, &stable).unwrap();
        assert_eq!(fallback_state, primary_state);
    }

    #[test]
    fn forecast_history_in_memory_flushes_once_and_suppresses_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let history_file = dir.path().join("history.json");
        let notifications_file = dir.path().join("notifications.jsonl");
        let forecast_cfg = ForecastConfig {
            enabled: true,
            max_observations_per_window: 10,
            stale_after_seconds: 600,
            notify_thresholds: vec![25.0, 10.0],
            history_path: Some(history_file.clone()),
            notifications_path: Some(notifications_file.clone()),
        };

        let mut history = load_forecast_history(&forecast_cfg);
        let state_for = |remaining_percent: f32| AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: vec![fresh_provider("codex", remaining_percent)],
        };

        // Cycle 1: 20% crosses 25% threshold -> 1 notification, 1 disk write.
        let mut cycle1_saves = 0usize;
        update_forecast_history_with(&forecast_cfg, &mut history, &state_for(20.0), |path, h| {
            cycle1_saves += 1;
            assert_eq!(h.windows[0].observations.len(), 1);
            assert_eq!(h.windows[0].notified.len(), 1);
            forecast::save_history(path, h)
        });
        assert_eq!(cycle1_saves, 1);

        // Cycle 2: 18% stays below 25% -> duplicate suppressed, still 1 disk write.
        let mut cycle2_saves = 0usize;
        update_forecast_history_with(&forecast_cfg, &mut history, &state_for(18.0), |path, h| {
            cycle2_saves += 1;
            assert_eq!(h.windows[0].observations.len(), 2);
            assert_eq!(h.windows[0].notified.len(), 1);
            forecast::save_history(path, h)
        });
        assert_eq!(cycle2_saves, 1);

        // Cycle 3: 8% crosses 10% threshold -> 1 new notification (25% suppressed), 1 disk write.
        let mut cycle3_saves = 0usize;
        update_forecast_history_with(&forecast_cfg, &mut history, &state_for(8.0), |path, h| {
            cycle3_saves += 1;
            assert_eq!(h.windows[0].observations.len(), 3);
            assert_eq!(h.windows[0].notified.len(), 2);
            forecast::save_history(path, h)
        });
        assert_eq!(cycle3_saves, 1);

        // Cycle 4: direct update_forecast_history call also keeps in-memory state and suppresses duplicates.
        update_forecast_history(&forecast_cfg, &mut history, &state_for(7.0));

        assert_eq!(history.windows.len(), 1);
        assert_eq!(history.windows[0].observations.len(), 4);
        assert_eq!(history.windows[0].notified.len(), 2);

        let loaded = forecast::load_history(&history_file).unwrap();
        assert_eq!(loaded, history);

        let log_contents = std::fs::read_to_string(&notifications_file).unwrap();
        let events: Vec<forecast::ThresholdEvent> = log_contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].threshold, 25.0);
        assert_eq!(events[0].remaining_percent, 20.0);
        assert_eq!(events[1].threshold, 10.0);
        assert_eq!(events[1].remaining_percent, 8.0);
    }

    #[test]
    fn single_provider_refresh_only_executes_matching_source_and_preserves_others() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let stable_path = dir.path().join("stable-state.json");
        let untouched_marker = dir.path().join("codex-executed.marker");

        let mut codex_existing = fresh_provider("codex", 75.0);
        let earlier = Utc::now() - Duration::minutes(10);
        codex_existing.updated_at = Some(earlier);

        let mut agy_existing = fresh_provider("antigravity", 60.0);
        agy_existing.updated_at = Some(earlier);

        let mut muse_existing = fresh_provider("muse", 40.0);
        muse_existing.updated_at = Some(earlier);

        let existing_state = AgentMeterState {
            version: STATE_VERSION,
            generated_at: earlier,
            providers: vec![codex_existing.clone(), agy_existing.clone(), muse_existing],
        };
        write_state(&state_path, &existing_state).unwrap();

        let muse_updated = fresh_provider("muse", 92.0);
        let codex_fresh = fresh_provider("codex", 10.0);
        let codex_json = serde_json::to_string(&codex_fresh).unwrap();
        let muse_json = serde_json::to_string(&muse_updated).unwrap();

        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 4,
            refresh_deadline_seconds: 5,
            sources: vec![
                Source::Command {
                    program: "sh".into(),
                    args: vec![
                        "-c".into(),
                        format!(
                            "touch '{}' && echo '{codex_json}'",
                            untouched_marker.display()
                        ),
                    ],
                    timeout_seconds: Some(5),
                },
                Source::Mock {
                    provider: fresh_provider("antigravity", 15.0),
                },
                Source::Command {
                    program: "sh".into(),
                    args: vec!["-c".into(), format!("echo '{muse_json}'")],
                    timeout_seconds: Some(5),
                },
            ],
            forecast: ForecastConfig::default(),
        };

        let loaded = load_existing_state_with_fallback(&state_path, &stable_path)
            .expect("existing state should load from state_path");
        let mut last_good = seed_last_good_from_state(&config, &loaded);

        let state = refresh_publishing_target(&config, Some("muse"), &mut last_good, |_| {});

        assert!(
            !untouched_marker.exists(),
            "non-targeted command source must not be executed"
        );
        assert_eq!(state.providers.len(), 3);
        assert_eq!(state.providers[0].id, "codex");
        assert_eq!(state.providers[0].windows[0].remaining_percent, Some(75.0));
        assert_eq!(state.providers[0].updated_at, Some(earlier));

        assert_eq!(state.providers[1].id, "antigravity");
        assert_eq!(state.providers[1].windows[0].remaining_percent, Some(60.0));
        assert_eq!(state.providers[1].updated_at, Some(earlier));

        assert_eq!(state.providers[2].id, "muse");
        assert_eq!(state.providers[2].windows[0].remaining_percent, Some(92.0));
        assert!(state.providers[2].updated_at.unwrap() > earlier);
    }

    #[test]
    fn single_provider_refresh_handles_agy_equivalence_and_stable_state_fallback() {
        let parsed = Args::try_parse_from(["agent-meterd", "--once", "--provider", "muse"])
            .expect("--provider flag should parse");
        assert_eq!(parsed.provider.as_deref(), Some("muse"));
        assert!(parsed.once);

        let dir = tempfile::tempdir().unwrap();
        let missing_runtime_state = dir.path().join("missing-runtime-state.json");
        let stable_path = dir.path().join("stable-state.json");
        let untouched_marker = dir.path().join("codex-executed.marker");

        let earlier = Utc::now() - Duration::minutes(5);
        let mut codex_existing = fresh_provider("codex", 88.0);
        codex_existing.updated_at = Some(earlier);
        let mut agy_existing = fresh_provider("antigravity", 30.0);
        agy_existing.updated_at = Some(earlier);

        let seed_state = AgentMeterState {
            version: STATE_VERSION,
            generated_at: earlier,
            providers: vec![codex_existing, agy_existing],
        };
        agent_meter::write_json_output(&stable_path, &seed_state).unwrap();

        let agy_refreshed = fresh_provider("antigravity", 97.0);
        let agy_json = serde_json::to_string(&agy_refreshed).unwrap();
        let script_path = dir.path().join("agent-meter-adapter");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"agy\" ]; then\n  echo '{agy_json}'\nelse\n  touch '{}'\nfi\n",
                untouched_marker.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }

        let config = Config {
            refresh_seconds: 60,
            command_timeout_seconds: 5,
            max_concurrent_refreshes: 4,
            refresh_deadline_seconds: 5,
            sources: vec![
                Source::Command {
                    program: script_path.to_string_lossy().into_owned(),
                    args: vec!["codex".into()],
                    timeout_seconds: Some(5),
                },
                Source::Command {
                    program: script_path.to_string_lossy().into_owned(),
                    args: vec!["agy".into()],
                    timeout_seconds: Some(5),
                },
            ],
            forecast: ForecastConfig::default(),
        };

        let loaded = load_existing_state_with_fallback(&missing_runtime_state, &stable_path)
            .expect("should fall back to stable state path");
        let mut last_good = seed_last_good_from_state(&config, &loaded);

        let state =
            refresh_publishing_target(&config, Some("antigravity"), &mut last_good, |_| {});

        assert!(
            !untouched_marker.exists(),
            "codex source must not run when refreshing antigravity"
        );
        assert_eq!(state.providers.len(), 2);
        assert_eq!(state.providers[0].id, "codex");
        assert_eq!(state.providers[0].windows[0].remaining_percent, Some(88.0));
        assert_eq!(state.providers[1].id, "antigravity");
        assert_eq!(state.providers[1].windows[0].remaining_percent, Some(97.0));
    }
}
