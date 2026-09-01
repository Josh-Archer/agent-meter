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
                .label(&format!("{:>3.0}%  {reset}", window.remaining_percent))
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
                    .label(&format!(
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
                .label(&format!("Waiting for Agent Meter daemon: {error}"))
                .wrap(true)
                .build(),
        ),
    }
}

fn main() {
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
