//! Shared, credential-free state for Agent Meter.
//!
//! Providers are deliberately external command or file sources. The daemon only
//! stores normalized availability data; it never persists credentials or raw
//! provider responses.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const STATE_FILE_NAME: &str = "state.json";
pub const STATE_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageWindow {
    /// Stable machine name, for example `five_hour` or `weekly`.
    pub id: String,
    pub label: String,
    /// Remaining allowance as a percentage from 0 through 100.
    pub remaining_percent: f32,
    /// Provider-supplied reset time, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_label: Option<String>,
}

impl UsageWindow {
    pub fn validate(&self) -> Result<()> {
        if !(0.0..=100.0).contains(&self.remaining_percent) {
            bail!("{} remaining_percent must be between 0 and 100", self.id);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderState {
    /// A safe lowercase identifier. It also selects a matching extension icon.
    pub id: String,
    pub label: String,
    /// Symbol name, such as `codex`. The GNOME extension maps this to local SVG.
    pub icon: String,
    pub windows: Vec<UsageWindow>,
    /// `fresh`, `stale`, `unavailable`, or `error`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_url: Option<String>,
}

impl ProviderState {
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            bail!("provider id must be lowercase letters, digits, or dashes");
        }
        if self.windows.is_empty() {
            bail!("{} has no usage windows", self.id);
        }
        for window in &self.windows {
            window.validate()?;
        }
        Ok(())
    }

    pub fn most_constrained_percent(&self) -> f32 {
        self.windows
            .iter()
            .map(|window| window.remaining_percent)
            .fold(100.0_f32, f32::min)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentMeterState {
    pub version: u8,
    pub generated_at: DateTime<Utc>,
    pub providers: Vec<ProviderState>,
}

impl AgentMeterState {
    pub fn validate(&self) -> Result<()> {
        if self.version != STATE_VERSION {
            bail!("unsupported state version {}", self.version);
        }
        let mut identifiers = HashSet::new();
        for provider in &self.providers {
            provider.validate()?;
            if !identifiers.insert(&provider.id) {
                bail!("duplicate provider id {}", provider.id);
            }
        }
        Ok(())
    }
}

pub fn runtime_state_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    runtime.join("agent-meter").join(STATE_FILE_NAME)
}

/// Persistent state path under `$XDG_STATE_HOME/agent-meter/state.json`.
///
/// Unlike `runtime_state_path`, this survives reboots.
pub fn stable_state_path() -> PathBuf {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/state")
        });
    state_home.join("agent-meter").join(STATE_FILE_NAME)
}

/// A single usage window in the public JSON schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonWindow {
    pub name: String,
    pub remaining_percent: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

/// A provider entry in the public JSON schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonProvider {
    pub provider: String,
    pub windows: Vec<JsonWindow>,
    pub updated_at: String,
    pub stale: bool,
}

/// Top-level public JSON output written by `agent-meter --json` and to the
/// stable state file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonOutput {
    pub schema_version: u8,
    pub providers: Vec<JsonProvider>,
}

impl JsonOutput {
    /// Build a `JsonOutput` from the internal daemon state.
    pub fn from_state(state: &AgentMeterState) -> Self {
        let providers = state
            .providers
            .iter()
            .map(|p| {
                let windows = p
                    .windows
                    .iter()
                    .map(|w| JsonWindow {
                        name: w.id.clone(),
                        remaining_percent: w.remaining_percent,
                        resets_at: w
                            .resets_at
                            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)),
                    })
                    .collect();
                let stale = p.status != "fresh";
                JsonProvider {
                    provider: p.id.clone(),
                    windows,
                    updated_at: state
                        .generated_at
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
                    stale,
                }
            })
            .collect();
        JsonOutput {
            schema_version: 1,
            providers,
        }
    }
}

pub fn read_state(path: &Path) -> Result<AgentMeterState> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("reading state from {}", path.display()))?;
    let state = serde_json::from_str::<AgentMeterState>(&contents)
        .with_context(|| format!("parsing state from {}", path.display()))?;
    state.validate()?;
    Ok(state)
}

/// Atomically replace the public state file so UI readers never observe a
/// partially-written JSON document.
pub fn write_state(path: &Path, state: &AgentMeterState) -> Result<()> {
    state.validate()?;
    let parent = path
        .parent()
        .context("state path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let temporary = parent.join(format!(".{}.{}.tmp", STATE_FILE_NAME, std::process::id()));
    let encoded = serde_json::to_vec_pretty(state)?;
    fs::write(&temporary, encoded).with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("replacing state at {}", path.display()))?;
    Ok(())
}

/// Atomically write the stable JsonOutput state under `$XDG_STATE_HOME/agent-meter/state.json`.
pub fn write_stable_state(state: &AgentMeterState) -> Result<PathBuf> {
    let path = stable_state_path();
    let output = JsonOutput::from_state(state);
    let parent = path
        .parent()
        .context("stable state path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let temporary = parent.join(format!(".{}.{}.tmp", STATE_FILE_NAME, std::process::id()));
    let encoded = serde_json::to_vec_pretty(&output)?;
    fs::write(&temporary, encoded).with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("replacing stable state at {}", path.display()))?;
    Ok(path)
}

/// Deterministic piecewise progressive color scale for remaining quota.
///
/// Returns a hex RGB color string, for example `#38d472`.
///
/// Stale/unavailable states return a neutral gray (`#8b949e`).
/// Percentages >= 40% return stable green (`#38d472`).
/// Percentages between 0% and 40% transition smoothly through yellow-green,
/// yellow, orange, and clear red.
pub fn quota_color_hex(percent: f32, is_fresh: bool) -> String {
    if !is_fresh {
        return "#8b949e".to_string();
    }
    let p = percent.clamp(0.0, 100.0);
    if p >= 40.0 {
        return "#38d472".to_string();
    }

    const STOPS: [(f32, (u8, u8, u8)); 5] = [
        (0.0, (255, 59, 86)),   // Red #ff3b56
        (10.0, (255, 102, 68)), // Red-Orange #ff6644
        (20.0, (255, 136, 51)), // Orange #ff8833
        (30.0, (246, 196, 69)), // Yellow #f6c445
        (40.0, (56, 212, 114)), // Green #38d472
    ];

    for i in 0..STOPS.len() - 1 {
        let (p0, c0) = STOPS[i];
        let (p1, c1) = STOPS[i + 1];
        if p >= p0 && p <= p1 {
            let t = (p - p0) / (p1 - p0);
            let r = (c0.0 as f32 + (c1.0 as f32 - c0.0 as f32) * t).round() as u8;
            let g = (c0.1 as f32 + (c1.1 as f32 - c0.1 as f32) * t).round() as u8;
            let b = (c0.2 as f32 + (c1.2 as f32 - c0.2 as f32) * t).round() as u8;
            return format!("#{r:02x}{g:02x}{b:02x}");
        }
    }

    "#ff3b56".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_state() -> AgentMeterState {
        AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: vec![ProviderState {
                id: "codex".into(),
                label: "Codex".into(),
                icon: "codex".into(),
                windows: vec![UsageWindow {
                    id: "five_hour".into(),
                    label: "5 hours".into(),
                    remaining_percent: 72.0,
                    resets_at: None,
                    reset_label: Some("in 1h 12m".into()),
                }],
                status: "fresh".into(),
                detail: None,
                usage_url: None,
            }],
        }
    }

    #[test]
    fn state_round_trips() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(STATE_FILE_NAME);
        let state = sample_state();
        write_state(&path, &state).unwrap();
        assert_eq!(read_state(&path).unwrap(), state);
    }

    #[test]
    fn invalid_percent_is_rejected() {
        let mut state = sample_state();
        state.providers[0].windows[0].remaining_percent = 101.0;
        assert!(state.validate().is_err());
    }

    #[test]
    fn duplicate_provider_is_rejected() {
        let mut state = sample_state();
        state.providers.push(state.providers[0].clone());
        assert!(state.validate().is_err());
    }

    #[test]
    fn progressive_quota_color_scale_stops() {
        assert_eq!(quota_color_hex(100.0, true), "#38d472");
        assert_eq!(quota_color_hex(41.0, true), "#38d472");
        assert_eq!(quota_color_hex(40.0, true), "#38d472");
        assert_eq!(quota_color_hex(30.0, true), "#f6c445");
        assert_eq!(quota_color_hex(20.0, true), "#ff8833");
        assert_eq!(quota_color_hex(10.0, true), "#ff6644");
        assert_eq!(quota_color_hex(0.0, true), "#ff3b56");
        assert_eq!(quota_color_hex(50.0, false), "#8b949e");
    }

    #[test]
    fn json_output_from_state() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-09-12T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let reset_ts = chrono::DateTime::parse_from_rfc3339("2026-09-12T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let state = AgentMeterState {
            version: STATE_VERSION,
            generated_at: ts,
            providers: vec![ProviderState {
                id: "codex".into(),
                label: "Codex".into(),
                icon: "codex".into(),
                windows: vec![UsageWindow {
                    id: "five_hour".into(),
                    label: "5 hours".into(),
                    remaining_percent: 72.0,
                    resets_at: Some(reset_ts),
                    reset_label: Some("in 1h".into()),
                }],
                status: "fresh".into(),
                detail: None,
                usage_url: None,
            }],
        };
        let output = JsonOutput::from_state(&state);
        assert_eq!(output.schema_version, 1);
        assert_eq!(output.providers.len(), 1);
        assert_eq!(output.providers[0].provider, "codex");
        assert!(!output.providers[0].stale);
        assert_eq!(output.providers[0].windows[0].name, "five_hour");
        assert_eq!(output.providers[0].windows[0].remaining_percent, 72.0);
        assert!(output.providers[0].windows[0].resets_at.is_some());
        let resets_at = output.providers[0].windows[0].resets_at.as_ref().unwrap();
        assert!(resets_at.contains("2026-09-12"));
    }

    #[test]
    fn json_output_marks_stale_providers() {
        let state = AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: vec![ProviderState {
                id: "grok".into(),
                label: "Grok".into(),
                icon: "grok".into(),
                windows: vec![UsageWindow {
                    id: "weekly".into(),
                    label: "Weekly".into(),
                    remaining_percent: 45.0,
                    resets_at: None,
                    reset_label: None,
                }],
                status: "stale".into(),
                detail: None,
                usage_url: None,
            }],
        };
        let output = JsonOutput::from_state(&state);
        assert!(output.providers[0].stale);
    }

    #[test]
    fn json_output_round_trips() {
        let state = sample_state();
        let output = JsonOutput::from_state(&state);
        let serialized = serde_json::to_string_pretty(&output).unwrap();
        let deserialized: JsonOutput = serde_json::from_str(&serialized).unwrap();
        assert_eq!(output, deserialized);
    }

    #[test]
    fn stable_state_path_respects_env() {
        let prev = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", "/tmp/test-state");
        let path = stable_state_path();
        if let Some(p) = prev {
            std::env::set_var("XDG_STATE_HOME", p);
        } else {
            std::env::remove_var("XDG_STATE_HOME");
        }
        assert_eq!(path, PathBuf::from("/tmp/test-state/agent-meter/state.json"));
    }

    #[test]
    fn json_output_schema_structure() {
        let state = sample_state();
        let output = JsonOutput::from_state(&state);
        let val: serde_json::Value = serde_json::to_value(&output).unwrap();
        assert_eq!(val["schema_version"], 1);
        let providers = val["providers"].as_array().expect("providers is array");
        assert!(!providers.is_empty());
        for p in providers {
            assert!(p["provider"].is_string());
            assert!(p["updated_at"].is_string());
            assert!(p["stale"].is_boolean());
            let windows = p["windows"].as_array().expect("windows is array");
            assert!(!windows.is_empty());
            for w in windows {
                assert!(w["name"].is_string());
                assert!(w["remaining_percent"].is_number());
                if let Some(resets_at) = w.get("resets_at") {
                    assert!(resets_at.is_string());
                    // Verify it parses as RFC3339/ISO8601
                    chrono::DateTime::parse_from_rfc3339(resets_at.as_str().unwrap()).unwrap();
                }
            }
        }
    }

    #[test]
    fn write_stable_state_writes_atomic_file() {
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let state = sample_state();
        let written_path = write_stable_state(&state).unwrap();
        assert!(written_path.exists());

        let content = fs::read_to_string(&written_path).unwrap();
        let parsed: JsonOutput = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.providers.len(), state.providers.len());

        if let Some(p) = prev {
            std::env::set_var("XDG_STATE_HOME", p);
        } else {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }
}
