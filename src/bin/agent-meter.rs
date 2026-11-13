use agent_meter::usage::{self, GroupBy, UsagePaths, UsageReport};
use agent_meter::{read_state, runtime_state_path, stable_state_path, AgentMeterState, JsonOutput};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};

fn load_json_output(custom_state: Option<&Path>) -> Result<JsonOutput, String> {
    let mut output = load_json_output_inner(custom_state)?;
    attach_forecasts(&mut output);
    Ok(output)
}

/// Best-effort: attach quota forecasts from the daemon's usage history when
/// forecasting is enabled. A missing or unreadable history leaves the output
/// unchanged.
fn attach_forecasts(output: &mut JsonOutput) {
    let path = agent_meter::forecast::default_history_path();
    let Ok(history) = agent_meter::forecast::load_history(&path) else {
        return;
    };
    if history.windows.is_empty() {
        return;
    }
    let forecasts = agent_meter::forecast::forecasts_for_json(
        &history,
        chrono::Utc::now(),
        agent_meter::forecast::DEFAULT_STALE_AFTER_SECS,
    );
    output.attach_forecasts(&forecasts);
}

fn load_json_output_inner(custom_state: Option<&Path>) -> Result<JsonOutput, String> {
    load_json_output_at(custom_state, &runtime_state_path(), &stable_state_path())
}

fn load_json_output_at(
    custom_state: Option<&Path>,
    runtime: &Path,
    stable: &Path,
) -> Result<JsonOutput, String> {
    if let Some(path) = custom_state {
        if let Ok(state) = read_state(path) {
            return Ok(JsonOutput::from_state(&state));
        }
        let content = fs::read_to_string(path)
            .map_err(|e| format!("reading state from {}: {e}", path.display()))?;
        if let Ok(output) = serde_json::from_str::<JsonOutput>(&content) {
            return Ok(output);
        }
        if let Ok(state) = serde_json::from_str::<AgentMeterState>(&content) {
            return Ok(JsonOutput::from_state(&state));
        }
        return Err(format!(
            "failed to parse state document from {}",
            path.display()
        ));
    }

    if runtime.exists() {
        if let Ok(state) = read_state(runtime) {
            return Ok(JsonOutput::from_state(&state));
        }
    }

    if stable.exists() {
        if let Ok(content) = fs::read_to_string(stable) {
            if let Ok(output) = serde_json::from_str::<JsonOutput>(&content) {
                return Ok(output);
            }
            if let Ok(state) = serde_json::from_str::<AgentMeterState>(&content) {
                return Ok(JsonOutput::from_state(&state));
            }
        }
    }

    Err(format!("reading state from {}", runtime.display()))
}

fn main() {
    // `usage` is a separate document from quota state. Handle it before the
    // quota --json path and before GTK, including `agent-meter usage --json`.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("usage") {
        match usage_main(&args[2..]) {
            Ok(report) => {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            }
            Err(err) => {
                eprintln!("agent-meter: {err}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.iter().any(|a| a == "--json") {
        let custom_state = args
            .windows(2)
            .find(|w| w[0] == "--state")
            .map(|w| PathBuf::from(&w[1]));

        match load_json_output(custom_state.as_deref()) {
            Ok(output) => {
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            }
            Err(err) => {
                eprintln!("agent-meter: {err}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Usage: agent-meter [OPTIONS]\n       agent-meter usage --json [--since 24h] [--by harness|model|host]\n\nOptions:\n  --json          Output provider state as JSON to stdout\n  --state <PATH>  Read quota state from a custom path\n  -h, --help      Print help\n\nUsage events are not part of state.json. `agent-meter usage --json` reads the\nlocal usage store. --since accepts 24h, 7d, 30m, or an RFC3339 timestamp.\n--usage-dir overrides $XDG_STATE_HOME/agent-meter."
        );
        return;
    }

    gui_main();
}

#[cfg(feature = "desktop")]
fn gui_main() {
    gui::run();
}

#[cfg(not(feature = "desktop"))]
fn gui_main() {
    eprintln!(
        "agent-meter: built without desktop feature; use --json or rebuild with --features desktop"
    );
    std::process::exit(1);
}

#[cfg(feature = "desktop")]
mod gui {
    use agent_meter::{read_state, runtime_state_path, AgentMeterState, ProviderState};
    use gtk::prelude::*;
    use gtk::{gdk, glib};

    fn style_for(percent: f32, status: &str) -> &'static str {
        if status != "fresh" {
            "dim"
        } else if percent <= 15.0 {
            "critical"
        } else if percent <= 35.0 {
            "warning"
        } else {
            "good"
        }
    }

    fn provider_card(provider: &ProviderState) -> gtk::Box {
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .css_classes(["provider-card"])
            .build();
        let heading = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        let name = gtk::Label::builder()
            .label(&provider.label)
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["provider-name"])
            .build();
        heading.append(&name);
        let availability = gtk::Label::builder()
            .label(&provider.status)
            .css_classes([style_for(
                provider.most_constrained_percent(),
                &provider.status,
            )])
            .build();
        heading.append(&availability);
        card.append(&heading);
        for window in &provider.windows {
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .build();
            row.append(
                &gtk::Label::builder()
                    .label(&window.label)
                    .width_chars(8)
                    .xalign(0.0)
                    .build(),
            );
            let Some(percent) = window.remaining_percent else {
                row.append(
                    &gtk::Label::builder()
                        .label("—")
                        .hexpand(true)
                        .xalign(0.0)
                        .build(),
                );
                card.append(&row);
                continue;
            };
            let bar = gtk::ProgressBar::builder()
                .fraction((percent / 100.0).into())
                .hexpand(true)
                .css_classes([style_for(percent, &provider.status)])
                .build();
            row.append(&bar);
            let reset = window.reset_label.as_deref().unwrap_or("no reset time");
            row.append(
                &gtk::Label::builder()
                    .label(format!("{:>3.0}%  {reset}", percent))
                    .xalign(1.0)
                    .build(),
            );
            card.append(&row);
        }
        if let Some(detail) = &provider.detail {
            card.append(
                &gtk::Label::builder()
                    .label(detail)
                    .xalign(0.0)
                    .wrap(true)
                    .css_classes(["detail"])
                    .build(),
            );
        }
        card
    }

    fn render(content: &gtk::Box, state: Result<AgentMeterState, anyhow::Error>) {
        while let Some(child) = content.first_child() {
            content.remove(&child);
        }
        if let Some(today) = agent_meter::usage::current_today_label() {
            content.append(
                &gtk::Label::builder()
                    .label(&today)
                    .xalign(0.0)
                    .css_classes(["detail"])
                    .build(),
            );
        }
        match state {
            Ok(state) => {
                for provider in &state.providers {
                    content.append(&provider_card(provider));
                }
                content.append(
                    &gtk::Label::builder()
                        .label(format!(
                            "Updated {}",
                            state.generated_at.format("%H:%M UTC")
                        ))
                        .xalign(0.0)
                        .css_classes(["detail"])
                        .build(),
                );
            }
            Err(error) => content.append(
                &gtk::Label::builder()
                    .label(format!("Waiting for Agent Meter daemon: {error}"))
                    .wrap(true)
                    .build(),
            ),
        }
    }

    pub fn run() {
        let app = gtk::Application::builder()
            .application_id("org.agentmeter.Widget")
            .build();
        app.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        provider.load_from_data(".provider-card { background: #24252b; border-radius: 12px; padding: 14px; } .provider-name { font-weight: 700; } .detail, .dim { color: #a8adb9; } .good { color: #3ddc97; } .warning { color: #ffbe55; } .critical { color: #ff6978; } window { background: #17181d; color: #f2f2f5; }");
        gtk::style_context_add_provider_for_display(&gdk::Display::default().expect("display"), &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    });
        app.connect_activate(|app| {
            let window = gtk::ApplicationWindow::builder()
                .application(app)
                .title("Agent Meter")
                .default_width(500)
                .default_height(430)
                .decorated(false)
                .resizable(false)
                .build();
            let root = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(12)
                .margin_top(16)
                .margin_bottom(16)
                .margin_start(16)
                .margin_end(16)
                .build();
            root.append(
                &gtk::Label::builder()
                    .label("Agent capacity")
                    .xalign(0.0)
                    .css_classes(["title-2"])
                    .build(),
            );
            let content = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(10)
                .build();
            root.append(&content);
            window.set_child(Some(&root));
            let state_path = runtime_state_path();
            render(&content, read_state(&state_path));
            glib::timeout_add_seconds_local(20, move || {
                render(&content, read_state(&state_path));
                glib::ControlFlow::Continue
            });
            window.present();
        });
        app.run();
    }
} // mod gui

fn usage_main(args: &[String]) -> Result<UsageReport, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "Usage: agent-meter usage --json [--since 24h] [--by harness|model|host] [--usage-dir PATH]\n\nReads cumulative session snapshots from the local usage store.\n--json is accepted and is the only output format.\n--since is a duration (30s, 15m, 24h, 7d, 2w) or an RFC3339 timestamp.\n--by groups totals by harness, model, or host."
        );
        std::process::exit(0);
    }
    let mut since_spec = None;
    let mut by_spec = None;
    let mut usage_dir = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--json" {
            index += 1;
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map(|(flag, value)| (flag, Some(value.to_owned())))
            .unwrap_or((arg.as_str(), None));
        let value = match flag {
            "--since" | "--by" | "--usage-dir" => {
                if let Some(value) = inline {
                    value
                } else {
                    index += 1;
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| format!("{flag} needs a value"))?
                }
            }
            other => return Err(format!("unknown argument {other}")),
        };
        match flag {
            "--since" => since_spec = Some(value),
            "--by" => by_spec = Some(value),
            "--usage-dir" => usage_dir = Some(PathBuf::from(value)),
            _ => unreachable!(),
        }
        index += 1;
    }
    let paths = match usage_dir {
        Some(dir) => UsagePaths::from_root(dir),
        None => UsagePaths::from_env(),
    };
    let store = usage::UsageStore::open(paths).map_err(|err| err.to_string())?;
    let since = match since_spec {
        Some(spec) => Some(usage::parse_since(&spec, Utc::now()).map_err(|err| err)?),
        None => None,
    };
    let group_by = match by_spec {
        Some(spec) => Some(GroupBy::parse(&spec)?),
        None => None,
    };
    Ok(store.query(since, group_by))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_meter::{write_json_output, write_state, ProviderState, UsageWindow, STATE_VERSION};
    use chrono::Utc;

    #[test]
    fn load_json_output_from_custom_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        let state = AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: vec![ProviderState {
                id: "codex".into(),
                label: "Codex".into(),
                icon: "codex".into(),
                windows: vec![UsageWindow {
                    id: "five_hour".into(),
                    label: "5 hours".into(),
                    remaining_percent: Some(80.0),
                    resets_at: None,
                    reset_label: None,
                }],
                status: "fresh".into(),
                detail: None,
                usage_url: None,
                reason: None,
                updated_at: None,
            }],
        };
        write_state(&path, &state).unwrap();
        let loaded = load_json_output(Some(&path)).unwrap();
        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.providers[0].provider, "codex");
        assert_eq!(loaded.providers[0].windows[0].remaining_percent, Some(80.0));
    }

    #[test]
    fn load_json_output_falls_back_to_stable_state() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("runtime").join("state.json");
        let stable = tmp
            .path()
            .join("state")
            .join("agent-meter")
            .join("state.json");

        let state = AgentMeterState {
            version: STATE_VERSION,
            generated_at: Utc::now(),
            providers: vec![ProviderState {
                id: "muse".into(),
                label: "Muse".into(),
                icon: "generic".into(),
                windows: vec![UsageWindow {
                    id: "weekly".into(),
                    label: "Weekly".into(),
                    remaining_percent: Some(92.0),
                    resets_at: None,
                    reset_label: None,
                }],
                status: "fresh".into(),
                detail: None,
                usage_url: None,
                reason: None,
                updated_at: None,
            }],
        };
        // Write only stable state, leaving runtime absent.
        write_json_output(&stable, &state).unwrap();

        let loaded = load_json_output_at(None, &runtime, &stable).unwrap();
        assert_eq!(loaded.providers[0].provider, "muse");
        assert_eq!(loaded.providers[0].windows[0].remaining_percent, Some(92.0));
        assert!(loaded.providers[0].available);
        assert!(!loaded.providers[0].stale);
    }

    #[test]
    fn usage_command_reads_the_store_without_touching_quota_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("agent-meter");
        let mut store = usage::UsageStore::open(UsagePaths::from_root(&root)).unwrap();
        let host = "homelabdesktop";
        let harness = "claude";
        let session = "session-1";
        let seq = 2u64;
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
            "repo": null
        });
        store
            .accept_payload(&serde_json::to_vec(&event).unwrap(), None)
            .unwrap();
        drop(store);

        let report = usage_main(&[
            "--json".into(),
            "--usage-dir".into(),
            root.display().to_string(),
            "--since".into(),
            "24h".into(),
            "--by".into(),
            "harness".into(),
        ])
        .unwrap();
        assert_eq!(report.schema, "agent-meter.usage-report/v1");
        assert_eq!(report.sessions, 1);
        assert_eq!(report.group_by.as_deref(), Some("harness"));
        assert_eq!(report.totals.input_tokens, Some(1200));
        assert_eq!(report.groups[0].key, "claude");
        assert!(!root.join("state.json").exists());
    }
}
