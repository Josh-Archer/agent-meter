//! Bounded, credential-free usage history and quota-exhaustion forecasting.
//!
//! The daemon records one observation per refresh for each `fresh` provider
//! window: a timestamp, the remaining percentage, and the provider-supplied
//! reset time. No credentials and no raw provider payloads are ever stored.
//!
//! Forecasts fit a line through the most recent observations and extrapolate
//! to zero remaining. When there are too few observations the forecast
//! reports `insufficient_data`; when the newest observation is older than the
//! configured staleness bound it reports `stale` instead of guessing.

use crate::{state_path_from, AgentMeterState};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub const HISTORY_VERSION: u8 = 1;
pub const HISTORY_FILE_NAME: &str = "usage-history.json";
pub const NOTIFICATIONS_FILE_NAME: &str = "notifications.jsonl";

/// Default cap on stored observations per (provider, window): 24h at a
/// 5-minute refresh cadence.
pub const DEFAULT_MAX_OBSERVATIONS: usize = 288;
/// Observations actually used for the linear fit: the most recent ones, so a
/// changing consumption rate is picked up quickly.
pub const FIT_OBSERVATION_COUNT: usize = 12;
/// Minimum observations required before a forecast is attempted.
pub const MIN_OBSERVATIONS: usize = 3;
/// Minimum span between the oldest and newest fitted observation.
pub const MIN_OBSERVATION_SPAN_SECS: i64 = 300;
/// An upward jump of this many percentage points is treated as a quota reset:
/// earlier observations belong to the previous cycle and are dropped.
pub const RESET_JUMP_PERCENT: f32 = 25.0;
/// Default staleness bound for the newest observation.
pub const DEFAULT_STALE_AFTER_SECS: i64 = 600;
/// Minimum R-squared for a forecast to be reported instead of `uncertain`.
pub const MIN_R_SQUARED: f64 = 0.5;
/// Slopes flatter than this (%/second) count as "not depleting".
pub const FLAT_SLOPE_EPSILON: f64 = 1e-9;

/// A single credential-free usage observation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageObservation {
    pub observed_at: DateTime<Utc>,
    /// Remaining allowance as a percentage from 0 through 100.
    pub remaining_percent: f32,
    /// Provider-supplied reset time, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
}

/// Records that a threshold notification was already sent for a quota cycle,
/// used for duplicate suppression.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThresholdNotice {
    pub threshold: f32,
    /// Identifies the quota cycle: the `resets_at` RFC3339 string or
    /// `"unknown"` when the provider reports no reset time.
    pub cycle: String,
}

/// Bounded history for one provider window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowHistory {
    pub provider: String,
    pub window: String,
    pub observations: VecDeque<UsageObservation>,
    #[serde(default)]
    pub notified: Vec<ThresholdNotice>,
}

impl WindowHistory {
    fn new(provider: &str, window: &str) -> Self {
        WindowHistory {
            provider: provider.to_owned(),
            window: window.to_owned(),
            observations: VecDeque::new(),
            notified: Vec::new(),
        }
    }
}

/// The whole persisted usage history document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UsageHistory {
    pub version: u8,
    #[serde(default)]
    pub windows: Vec<WindowHistory>,
}

impl UsageHistory {
    pub fn window_mut(&mut self, provider: &str, window: &str) -> &mut WindowHistory {
        let position = self
            .windows
            .iter()
            .position(|w| w.provider == provider && w.window == window);
        match position {
            Some(index) => &mut self.windows[index],
            None => {
                self.windows.push(WindowHistory::new(provider, window));
                self.windows.last_mut().expect("just pushed")
            }
        }
    }

    pub fn window(&self, provider: &str, window: &str) -> Option<&WindowHistory> {
        self.windows
            .iter()
            .find(|w| w.provider == provider && w.window == window)
    }
}

/// Forecast outcome for one provider window.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForecastStatus {
    /// Too few observations, or they span too little time.
    InsufficientData,
    /// The newest observation is older than the staleness bound.
    Stale,
    /// A trend exists but the fit is too noisy to trust.
    Uncertain,
    /// A usable forecast (or a confident "not depleting" verdict).
    Ok,
}

/// Machine-readable forecast attached to a JSON usage window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonForecast {
    pub status: ForecastStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_exhaustion_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhausts_before_reset: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumption_rate_percent_per_hour: Option<f64>,
    /// True when remaining quota is flat or increasing.
    #[serde(default)]
    pub depleting: bool,
    pub observation_count: usize,
}

/// Internal forecast before JSON rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowForecast {
    pub status: ForecastStatus,
    pub estimated_exhaustion_at: Option<DateTime<Utc>>,
    pub resets_at: Option<DateTime<Utc>>,
    pub exhausts_before_reset: Option<bool>,
    pub consumption_rate_percent_per_hour: Option<f64>,
    pub depleting: bool,
    pub observation_count: usize,
}

impl WindowForecast {
    pub fn to_json(&self) -> JsonForecast {
        JsonForecast {
            status: self.status,
            estimated_exhaustion_at: self.estimated_exhaustion_at.map(|dt| {
                dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
            }),
            exhausts_before_reset: self.exhausts_before_reset,
            consumption_rate_percent_per_hour: self.consumption_rate_percent_per_hour,
            depleting: self.depleting,
            observation_count: self.observation_count,
        }
    }
}

fn cycle_key(resets_at: Option<DateTime<Utc>>) -> String {
    resets_at
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Record one observation, enforcing the per-window bound and detecting quota
/// resets. A reset starts a fresh cycle: older observations and threshold
/// notification records are dropped.
pub fn record_observation(
    history: &mut UsageHistory,
    provider: &str,
    window: &str,
    observation: UsageObservation,
    max_observations: usize,
) {
    let entry = history.window_mut(provider, window);
    let reset = entry
        .observations
        .back()
        .map(|previous| {
            observation.remaining_percent - previous.remaining_percent >= RESET_JUMP_PERCENT
        })
        .unwrap_or(false);
    if reset {
        entry.observations.clear();
        entry.notified.clear();
    }
    entry.observations.push_back(observation);
    let max = max_observations.max(1);
    while entry.observations.len() > max {
        entry.observations.pop_front();
    }
}

/// Record observations for every `fresh` provider window in a daemon refresh.
pub fn record_state(
    history: &mut UsageHistory,
    state: &AgentMeterState,
    now: DateTime<Utc>,
    max_observations: usize,
) {
    for provider in &state.providers {
        if provider.status != "fresh" {
            continue;
        }
        for window in &provider.windows {
            record_observation(
                history,
                &provider.id,
                &window.id,
                UsageObservation {
                    observed_at: now,
                    remaining_percent: window.remaining_percent,
                    resets_at: window.resets_at,
                },
                max_observations,
            );
        }
    }
}

/// Least-squares fit of remaining percent against elapsed seconds.
/// Returns `(slope_percent_per_second, r_squared)`.
fn linear_fit(points: &[(f64, f64)]) -> (f64, f64) {
    let n = points.len() as f64;
    let mean_x = points.iter().map(|p| p.0).sum::<f64>() / n;
    let mean_y = points.iter().map(|p| p.1).sum::<f64>() / n;
    let mut ss_xy = 0.0;
    let mut ss_xx = 0.0;
    let mut ss_yy = 0.0;
    for (x, y) in points {
        let dx = x - mean_x;
        let dy = y - mean_y;
        ss_xy += dx * dy;
        ss_xx += dx * dx;
        ss_yy += dy * dy;
    }
    if ss_xx == 0.0 {
        return (0.0, 0.0);
    }
    let slope = ss_xy / ss_xx;
    let r_squared = if ss_yy == 0.0 {
        1.0
    } else {
        let r = ss_xy / (ss_xx * ss_yy).sqrt();
        r * r
    };
    (slope, r_squared)
}

/// Forecast quota exhaustion for one window's history.
pub fn forecast_window(
    entry: &WindowHistory,
    now: DateTime<Utc>,
    stale_after_secs: i64,
) -> WindowForecast {
    let observation_count = entry.observations.len();
    let base = WindowForecast {
        status: ForecastStatus::InsufficientData,
        estimated_exhaustion_at: None,
        resets_at: entry.observations.back().and_then(|o| o.resets_at),
        exhausts_before_reset: None,
        consumption_rate_percent_per_hour: None,
        depleting: false,
        observation_count,
    };
    if observation_count < MIN_OBSERVATIONS {
        return base;
    }
    let newest = entry.observations.back().expect("non-empty");
    if (now - newest.observed_at).num_seconds() > stale_after_secs {
        return WindowForecast {
            status: ForecastStatus::Stale,
            ..base
        };
    }
    let oldest = entry.observations.front().expect("non-empty");
    if (newest.observed_at - oldest.observed_at).num_seconds() < MIN_OBSERVATION_SPAN_SECS {
        return base;
    }

    let fit_count = FIT_OBSERVATION_COUNT.min(observation_count);
    let start = observation_count - fit_count;
    let t0 = entry.observations[start].observed_at;
    let points: Vec<(f64, f64)> = entry
        .observations
        .iter()
        .skip(start)
        .map(|o| {
            (
                (o.observed_at - t0).num_seconds() as f64,
                o.remaining_percent as f64,
            )
        })
        .collect();
    let (slope, r_squared) = linear_fit(&points);
    let rate_per_hour = slope * 3600.0;

    if slope >= -FLAT_SLOPE_EPSILON {
        // Flat or increasing: quota is not depleting.
        return WindowForecast {
            status: ForecastStatus::Ok,
            consumption_rate_percent_per_hour: Some(rate_per_hour),
            ..base
        };
    }
    if !r_squared.is_finite() || r_squared < MIN_R_SQUARED {
        return WindowForecast {
            status: ForecastStatus::Uncertain,
            consumption_rate_percent_per_hour: Some(rate_per_hour),
            ..base
        };
    }

    // Extrapolate the fitted line to zero remaining.
    let last = points.last().expect("non-empty");
    let seconds_to_zero = last.1 / -slope;
    let estimated = newest.observed_at
        + chrono::Duration::seconds(seconds_to_zero.max(0.0).round() as i64);
    let exhausts_before_reset = base.resets_at.map(|reset| estimated < reset);
    WindowForecast {
        status: ForecastStatus::Ok,
        estimated_exhaustion_at: Some(estimated),
        exhausts_before_reset,
        consumption_rate_percent_per_hour: Some(rate_per_hour),
        depleting: true,
        ..base
    }
}

/// A threshold crossing ready to notify.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThresholdEvent {
    pub notified_at: DateTime<Utc>,
    pub provider: String,
    pub window: String,
    pub threshold: f32,
    pub remaining_percent: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_exhaustion_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

/// Evaluate notification thresholds for one window, recording notices for
/// duplicate suppression. Thresholds fire when remaining drops at or below
/// them, once per quota cycle.
pub fn check_window_thresholds(
    entry: &mut WindowHistory,
    thresholds: &[f32],
    now: DateTime<Utc>,
    stale_after_secs: i64,
) -> Vec<ThresholdEvent> {
    let mut events = Vec::new();
    let Some(newest) = entry.observations.back().cloned() else {
        return events;
    };
    if (now - newest.observed_at).num_seconds() > stale_after_secs {
        return events;
    }
    let forecast = forecast_window(entry, now, stale_after_secs);
    let cycle = cycle_key(newest.resets_at);
    let mut sorted: Vec<f32> = thresholds.to_vec();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    sorted.dedup_by(|a, b| (*a - *b).abs() < f32::EPSILON);
    for threshold in sorted {
        if newest.remaining_percent > threshold {
            continue;
        }
        if entry
            .notified
            .iter()
            .any(|n| (n.threshold - threshold).abs() < f32::EPSILON && n.cycle == cycle)
        {
            continue;
        }
        entry.notified.push(ThresholdNotice { threshold, cycle: cycle.clone() });
        events.push(ThresholdEvent {
            notified_at: now,
            provider: entry.provider.clone(),
            window: entry.window.clone(),
            threshold,
            remaining_percent: newest.remaining_percent,
            estimated_exhaustion_at: forecast.estimated_exhaustion_at.map(|dt| {
                dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
            }),
            resets_at: newest.resets_at.map(|dt| {
                dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
            }),
        });
    }
    events
}

/// Evaluate thresholds across all windows in the history.
pub fn check_notifications(
    history: &mut UsageHistory,
    thresholds: &[f32],
    now: DateTime<Utc>,
    stale_after_secs: i64,
) -> Vec<ThresholdEvent> {
    let mut events = Vec::new();
    for entry in history.windows.iter_mut() {
        events.extend(check_window_thresholds(entry, thresholds, now, stale_after_secs));
    }
    events
}

/// Forecasts keyed by (provider, window), ready to attach to JSON output.
pub fn forecasts_for_json(
    history: &UsageHistory,
    now: DateTime<Utc>,
    stale_after_secs: i64,
) -> HashMap<(String, String), JsonForecast> {
    history
        .windows
        .iter()
        .map(|entry| {
            (
                (entry.provider.clone(), entry.window.clone()),
                forecast_window(entry, now, stale_after_secs).to_json(),
            )
        })
        .collect()
}

/// Resolve the default usage-history path from explicit env values, mirroring
/// the stable state location: `$XDG_STATE_HOME/agent-meter/usage-history.json`.
pub fn history_path_from(
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> PathBuf {
    let state_path = state_path_from(xdg_state_home, home);
    state_path
        .parent()
        .map(|parent| parent.join(HISTORY_FILE_NAME))
        .unwrap_or_else(|| PathBuf::from(HISTORY_FILE_NAME))
}

/// Default usage-history path under `$XDG_STATE_HOME/agent-meter/`.
pub fn default_history_path() -> PathBuf {
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty());
    history_path_from(std::env::var_os("XDG_STATE_HOME"), home)
}

/// Default notifications log path under `$XDG_STATE_HOME/agent-meter/`.
pub fn default_notifications_path() -> PathBuf {
    default_history_path()
        .parent()
        .map(|parent| parent.join(NOTIFICATIONS_FILE_NAME))
        .unwrap_or_else(|| PathBuf::from(NOTIFICATIONS_FILE_NAME))
}

/// Load usage history; a missing file yields an empty history.
pub fn load_history(path: &Path) -> Result<UsageHistory> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UsageHistory {
                version: HISTORY_VERSION,
                windows: Vec::new(),
            })
        }
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let history: UsageHistory =
        serde_json::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
    Ok(history)
}

/// Atomically replace the usage-history file.
pub fn save_history(path: &Path, history: &UsageHistory) -> Result<()> {
    let parent = path
        .parent()
        .context("history path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let temporary = parent.join(format!(".{}.{}.tmp", HISTORY_FILE_NAME, std::process::id()));
    let encoded = serde_json::to_vec_pretty(history)?;
    fs::write(&temporary, encoded).with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("replacing history at {}", path.display()))?;
    Ok(())
}

/// Where threshold notifications go.
pub trait NotifySink {
    fn notify(&mut self, event: &ThresholdEvent);
}

/// Appends threshold events as JSON lines to a log file and echoes a
/// one-line summary to stderr. Credential-free: events carry only
/// percentages, timestamps, and threshold values.
pub struct FileLogSink {
    path: PathBuf,
}

impl FileLogSink {
    pub fn new(path: PathBuf) -> Self {
        FileLogSink { path }
    }
}

impl NotifySink for FileLogSink {
    fn notify(&mut self, event: &ThresholdEvent) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(line) = serde_json::to_string(event) {
            use std::io::Write;
            if let Ok(mut file) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
        eprintln!(
            "agent-meterd: quota notification: {}/{} at {:.1}% remaining (threshold {:.1}%)",
            event.provider, event.window, event.remaining_percent, event.threshold
        );
    }
}

/// In-memory sink for tests.
#[cfg(test)]
pub struct VecSink {
    pub events: Vec<ThresholdEvent>,
}

#[cfg(test)]
impl NotifySink for VecSink {
    fn notify(&mut self, event: &ThresholdEvent) {
        self.events.push(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn obs(hours_after: i64, remaining: f32, base: DateTime<Utc>) -> UsageObservation {
        UsageObservation {
            observed_at: base + chrono::Duration::hours(hours_after),
            remaining_percent: remaining,
            resets_at: Some(base + chrono::Duration::hours(12)),
        }
    }

    fn base_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
    }

    fn declining_history() -> (UsageHistory, DateTime<Utc>) {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        // 10%/hour decline: 100 -> 0 over 10 hours.
        for (h, remaining) in [(0, 100.0), (1, 90.0), (2, 80.0), (3, 70.0)] {
            record_observation(&mut history, "codex", "five_hour", obs(h, remaining, base), 288);
        }
        (history, base + chrono::Duration::hours(3))
    }

    #[test]
    fn history_is_bounded_per_window() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        for h in 0..50 {
            record_observation(
                &mut history,
                "codex",
                "five_hour",
                obs(h, 100.0 - h as f32, base),
                10,
            );
        }
        let entry = history.window("codex", "five_hour").unwrap();
        assert_eq!(entry.observations.len(), 10);
        assert_eq!(entry.observations.front().unwrap().remaining_percent, 60.0);
        assert_eq!(entry.observations.back().unwrap().remaining_percent, 51.0);
    }

    #[test]
    fn observations_are_credential_free() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        record_observation(&mut history, "codex", "five_hour", obs(0, 72.0, base), 288);
        let value = serde_json::to_value(&history).unwrap();
        let serialized = serde_json::to_string(&value).unwrap();
        for forbidden in ["token", "key", "secret", "password", "credential", "auth"] {
            assert!(
                !serialized.to_lowercase().contains(forbidden),
                "history must stay credential-free, found {forbidden}"
            );
        }
        let observation = &value["windows"][0]["observations"][0];
        let keys: Vec<&str> = observation
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["observed_at", "remaining_percent", "resets_at"]);
    }

    #[test]
    fn reset_jump_starts_a_new_cycle_and_clears_notices() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        for (h, remaining) in [(0, 80.0), (1, 70.0), (2, 60.0)] {
            record_observation(&mut history, "codex", "five_hour", obs(h, remaining, base), 288);
        }
        let entry = history.window_mut("codex", "five_hour");
        entry.notified.push(ThresholdNotice {
            threshold: 75.0,
            cycle: "old".into(),
        });
        // Quota reset: jumps back up.
        record_observation(&mut history, "codex", "five_hour", obs(3, 98.0, base), 288);
        let entry = history.window("codex", "five_hour").unwrap();
        assert_eq!(entry.observations.len(), 1);
        assert_eq!(entry.observations[0].remaining_percent, 98.0);
        assert!(entry.notified.is_empty());
    }

    #[test]
    fn small_recovery_is_not_a_reset() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        for (h, remaining) in [(0, 80.0), (1, 70.0), (2, 72.0)] {
            record_observation(&mut history, "codex", "five_hour", obs(h, remaining, base), 288);
        }
        let entry = history.window("codex", "five_hour").unwrap();
        assert_eq!(entry.observations.len(), 3);
    }

    #[test]
    fn forecast_with_too_few_observations_is_insufficient() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        record_observation(&mut history, "codex", "five_hour", obs(0, 90.0, base), 288);
        record_observation(&mut history, "codex", "five_hour", obs(1, 85.0, base), 288);
        let entry = history.window("codex", "five_hour").unwrap();
        let forecast = forecast_window(entry, base + chrono::Duration::hours(1), 600);
        assert_eq!(forecast.status, ForecastStatus::InsufficientData);
        assert_eq!(forecast.observation_count, 2);
        assert!(forecast.estimated_exhaustion_at.is_none());
    }

    #[test]
    fn forecast_needs_minimum_span() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        for m in [0, 1, 2] {
            record_observation(
                &mut history,
                "codex",
                "five_hour",
                UsageObservation {
                    observed_at: base + chrono::Duration::minutes(m),
                    remaining_percent: 90.0 - m as f32,
                    resets_at: None,
                },
                288,
            );
        }
        let entry = history.window("codex", "five_hour").unwrap();
        let forecast = forecast_window(entry, base + chrono::Duration::minutes(2), 600);
        assert_eq!(forecast.status, ForecastStatus::InsufficientData);
    }

    #[test]
    fn forecast_estimates_exhaustion_before_reset() {
        let (history, now) = declining_history();
        let entry = history.window("codex", "five_hour").unwrap();
        let forecast = forecast_window(entry, now, 3600 * 5);
        assert_eq!(forecast.status, ForecastStatus::Ok);
        assert!(forecast.depleting);
        let estimated = forecast.estimated_exhaustion_at.unwrap();
        // 70% at 10%/hour -> exhausted ~7h after the 3h mark, i.e. t=10h.
        let expected = base_time() + chrono::Duration::hours(10);
        assert!(
            (estimated - expected).num_seconds().abs() <= 60,
            "estimated {estimated} should be near {expected}"
        );
        // Reset is at t=12h, after exhaustion.
        assert_eq!(forecast.exhausts_before_reset, Some(true));
        let rate = forecast.consumption_rate_percent_per_hour.unwrap();
        assert!((rate + 10.0).abs() < 0.01, "rate was {rate}");
    }

    #[test]
    fn forecast_safe_when_reset_comes_first() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        for (h, remaining) in [(0, 100.0), (1, 99.0), (2, 98.0)] {
            record_observation(
                &mut history,
                "codex",
                "five_hour",
                UsageObservation {
                    observed_at: base + chrono::Duration::hours(h),
                    remaining_percent: remaining,
                    resets_at: Some(base + chrono::Duration::hours(3)),
                },
                288,
            );
        }
        let entry = history.window("codex", "five_hour").unwrap();
        let forecast = forecast_window(entry, base + chrono::Duration::hours(2), 3600 * 5);
        assert_eq!(forecast.status, ForecastStatus::Ok);
        assert_eq!(forecast.exhausts_before_reset, Some(false));
    }

    #[test]
    fn stale_observation_beats_uncertain_prediction() {
        let (history, _) = declining_history();
        let entry = history.window("codex", "five_hour").unwrap();
        // Newest observation is 3h old; bound is 10 minutes.
        let forecast = forecast_window(entry, base_time() + chrono::Duration::hours(6), 600);
        assert_eq!(forecast.status, ForecastStatus::Stale);
        assert!(forecast.estimated_exhaustion_at.is_none());
    }

    #[test]
    fn flat_usage_is_not_depleting() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        for h in [0, 1, 2, 3] {
            record_observation(&mut history, "codex", "five_hour", obs(h, 80.0, base), 288);
        }
        let entry = history.window("codex", "five_hour").unwrap();
        let forecast = forecast_window(entry, base + chrono::Duration::hours(3), 3600 * 5);
        assert_eq!(forecast.status, ForecastStatus::Ok);
        assert!(!forecast.depleting);
        assert!(forecast.estimated_exhaustion_at.is_none());
    }

    #[test]
    fn noisy_trend_is_uncertain() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        // Jagged but slightly declining: poor linear fit. Upward jumps stay
        // below the reset-detection threshold so the history is kept whole.
        for (h, remaining) in [
            (0, 100.0),
            (1, 85.0),
            (2, 98.0),
            (3, 80.0),
            (4, 95.0),
            (5, 75.0),
        ] {
            record_observation(&mut history, "codex", "five_hour", obs(h, remaining, base), 288);
        }
        let entry = history.window("codex", "five_hour").unwrap();
        let forecast = forecast_window(entry, base + chrono::Duration::hours(5), 3600 * 8);
        assert_eq!(forecast.status, ForecastStatus::Uncertain);
    }

    #[test]
    fn changing_rate_uses_recent_observations() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        // Old regime: 1%/hour for 8 hours, then a fast burn: 20%/hour.
        for h in 0..8 {
            record_observation(
                &mut history,
                "codex",
                "five_hour",
                obs(h, 100.0 - h as f32, base),
                288,
            );
        }
        for (h, remaining) in [(8, 92.0), (9, 72.0), (10, 52.0), (11, 32.0), (12, 12.0)] {
            record_observation(&mut history, "codex", "five_hour", obs(h, remaining, base), 288);
        }
        let entry = history.window("codex", "five_hour").unwrap();
        let forecast = forecast_window(entry, base + chrono::Duration::hours(12), 3600 * 30);
        assert_eq!(forecast.status, ForecastStatus::Ok);
        let rate = forecast.consumption_rate_percent_per_hour.unwrap();
        assert!(
            rate < -5.0,
            "recent fast burn should dominate the fit, got {rate}"
        );
    }

    #[test]
    fn thresholds_fire_once_per_cycle() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        for (h, remaining) in [(0, 100.0), (1, 80.0), (2, 60.0), (3, 20.0)] {
            record_observation(&mut history, "codex", "five_hour", obs(h, remaining, base), 288);
        }
        let now = base + chrono::Duration::hours(3);
        let entry = history.window_mut("codex", "five_hour");
        let events = check_window_thresholds(entry, &[25.0, 10.0], now, 3600 * 5);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].threshold, 25.0);
        assert_eq!(events[0].remaining_percent, 20.0);

        // Second evaluation: suppressed as duplicate.
        let entry = history.window_mut("codex", "five_hour");
        let events = check_window_thresholds(entry, &[25.0, 10.0], now, 3600 * 5);
        assert!(events.is_empty());

        // New cycle after a reset: fires again.
        record_observation(&mut history, "codex", "five_hour", obs(4, 100.0, base), 288);
        record_observation(&mut history, "codex", "five_hour", obs(5, 90.0, base), 288);
        record_observation(&mut history, "codex", "five_hour", obs(6, 20.0, base), 288);
        let now = base + chrono::Duration::hours(6);
        let entry = history.window_mut("codex", "five_hour");
        let events = check_window_thresholds(entry, &[25.0, 10.0], now, 3600 * 8);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].threshold, 25.0);
    }

    #[test]
    fn thresholds_do_not_fire_above_threshold_or_when_stale() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        for (h, remaining) in [(0, 100.0), (1, 90.0), (2, 80.0)] {
            record_observation(&mut history, "codex", "five_hour", obs(h, remaining, base), 288);
        }
        let now = base + chrono::Duration::hours(2);
        let entry = history.window_mut("codex", "five_hour");
        assert!(check_window_thresholds(entry, &[25.0], now, 3600).is_empty());

        // Stale data: no notification even below threshold.
        let entry = history.window_mut("codex", "five_hour");
        entry.observations.back_mut().unwrap().remaining_percent = 5.0;
        let stale_now = base + chrono::Duration::hours(20);
        let events = check_window_thresholds(entry, &[25.0], stale_now, 600);
        assert!(events.is_empty());
    }

    #[test]
    fn notifications_are_opt_in_via_threshold_list() {
        let base = base_time();
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        for (h, remaining) in [(0, 100.0), (1, 80.0), (2, 60.0), (3, 20.0)] {
            record_observation(&mut history, "codex", "five_hour", obs(h, remaining, base), 288);
        }
        let now = base + chrono::Duration::hours(3);
        // Empty thresholds: nothing fires.
        let events = check_notifications(&mut history, &[], now, 3600 * 5);
        assert!(events.is_empty());
    }

    #[test]
    fn history_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HISTORY_FILE_NAME);
        let (history, _) = declining_history();
        save_history(&path, &history).unwrap();
        let loaded = load_history(&path).unwrap();
        assert_eq!(loaded, history);
    }

    #[test]
    fn load_history_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_history(&dir.path().join("nope.json")).unwrap();
        assert_eq!(loaded.version, HISTORY_VERSION);
        assert!(loaded.windows.is_empty());
    }

    #[test]
    fn record_state_only_tracks_fresh_providers() {
        use crate::{AgentMeterState, ProviderState, UsageWindow, STATE_VERSION};
        let base = base_time();
        let state = AgentMeterState {
            version: STATE_VERSION,
            generated_at: base,
            providers: vec![
                ProviderState {
                    id: "fresh-one".into(),
                    label: "Fresh".into(),
                    icon: "generic".into(),
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
                },
                ProviderState {
                    id: "stale-one".into(),
                    label: "Stale".into(),
                    icon: "generic".into(),
                    windows: vec![UsageWindow {
                        id: "daily".into(),
                        label: "Daily".into(),
                        remaining_percent: 50.0,
                        resets_at: None,
                        reset_label: None,
                    }],
                    status: "stale".into(),
                    detail: None,
                    usage_url: None,
                },
            ],
        };
        let mut history = UsageHistory {
            version: HISTORY_VERSION,
            windows: Vec::new(),
        };
        record_state(&mut history, &state, base, 288);
        assert!(history.window("fresh-one", "daily").is_some());
        assert!(history.window("stale-one", "daily").is_none());
    }

    #[test]
    fn history_path_from_mirrors_stable_state_location() {
        let path = history_path_from(
            Some(OsString::from("/tmp/test-state")),
            Some(OsString::from("/home/someone")),
        );
        assert_eq!(
            path,
            PathBuf::from("/tmp/test-state/agent-meter").join(HISTORY_FILE_NAME)
        );
    }

    #[test]
    fn forecasts_for_json_keys_by_provider_and_window() {
        let (history, now) = declining_history();
        let map = forecasts_for_json(&history, now, 3600 * 5);
        let key = ("codex".to_string(), "five_hour".to_string());
        let forecast = map.get(&key).expect("forecast present");
        assert_eq!(forecast.status, ForecastStatus::Ok);
        assert_eq!(forecast.observation_count, 4);
        assert!(forecast.estimated_exhaustion_at.is_some());
    }
}
