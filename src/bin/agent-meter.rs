use agent_meter::{read_state, runtime_state_path, stable_state_path, AgentMeterState, JsonOutput};
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
    // Handle --json before initializing GTK.
    let args: Vec<String> = std::env::args().collect();
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
        println!("Usage: agent-meter [OPTIONS]\n\nOptions:\n  --json          Output provider state as JSON to stdout\n  --state <PATH>  Read state from custom path\n  -h, --help      Print help");
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
    eprintln!("agent-meter: built without desktop feature; use --json or rebuild with --features desktop");
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
        let bar = gtk::ProgressBar::builder()
            .fraction((window.remaining_percent / 100.0).into())
            .hexpand(true)
            .css_classes([style_for(window.remaining_percent, &provider.status)])
            .build();
        row.append(&bar);
        let reset = window.reset_label.as_deref().unwrap_or("no reset time");
        row.append(
            &gtk::Label::builder()
                .label(format!("{:>3.0}%  {reset}", window.remaining_percent))
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
                    remaining_percent: 80.0,
                    resets_at: None,
                    reset_label: None,
                }],
                status: "fresh".into(),
                detail: None,
                usage_url: None,
            }],
        };
        write_state(&path, &state).unwrap();
        let loaded = load_json_output(Some(&path)).unwrap();
        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.providers[0].provider, "codex");
        assert_eq!(loaded.providers[0].windows[0].remaining_percent, 80.0);
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
                    remaining_percent: 92.0,
                    resets_at: None,
                    reset_label: None,
                }],
                status: "fresh".into(),
                detail: None,
                usage_url: None,
            }],
        };
        // Write only stable state, leaving runtime absent.
        write_json_output(&stable, &state).unwrap();

        let loaded = load_json_output_at(None, &runtime, &stable).unwrap();
        assert_eq!(loaded.providers[0].provider, "muse");
        assert_eq!(loaded.providers[0].windows[0].remaining_percent, 92.0);
        assert!(loaded.providers[0].available);
        assert!(!loaded.providers[0].stale);
    }
}
