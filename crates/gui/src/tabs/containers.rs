use crate::{
    format::fmt_duration,
    parse::{Container, State},
    state::{Cmd, Data, Kind, Task, effective_state},
    theme,
};
use eframe::egui::{self, RichText, Ui};

#[derive(Default)]
pub struct UiState {
    pub filter: String,
}

fn status_text(c: &Container, state: State) -> String {
    let mut s = theme::state_text(state).to_string();
    match state {
        State::Running | State::Paused => {
            if let Some(u) = c.uptime {
                s = format!("{s} {}", fmt_duration(u));
            }
        }
        State::Exited => {
            if let Some(code) = c.exit_code {
                s = format!("Exited ({code})");
            }
        }
        State::Restarting => {
            if let Some(n) = c.restarts {
                s = format!("Restarting ({n})");
            }
        }
        State::Created => {}
    }
    match c.health.as_deref() {
        Some(h) if state == State::Running => format!("{s} · {h}"),
        _ => s,
    }
}

pub fn matches(c: &Container, filter: &str) -> bool {
    let f = filter.trim().to_lowercase();
    f.is_empty() || [&c.name, &c.id, &c.image, &c.ip, &c.ports].iter().any(|x| x.to_lowercase().contains(&f))
}

pub fn show(ui: &mut Ui, data: &Data, st: &mut UiState, out: &mut Vec<Cmd>) {
    ui.horizontal(|ui| {
        ui.heading("Containers");
        ui.add_space(8.0);
        ui.add(egui::TextEdit::singleline(&mut st.filter).hint_text("🔍 Filter").desired_width(200.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::primary_button(ui, "＋ Run container") {
                out.push(Cmd::RunDialog(String::new()));
            }
            let stopped: Vec<&Container> = data.containers.iter().filter(|c| !c.is_live()).collect();
            if ui.add_enabled(!stopped.is_empty(), egui::Button::new("🗑 Remove stopped")).clicked() {
                let steps = stopped.iter().map(|c| crate::worker::args(&["rm", &c.id])).collect();
                out.push(Cmd::Confirm(format!("Remove all {} stopped containers?", stopped.len()), Task { label: "Remove stopped containers".into(), steps }));
            }
        });
    });
    ui.separator();

    let mut rows: Vec<&Container> = data.containers.iter().filter(|c| matches(c, &st.filter)).collect();
    rows.sort_by_key(|c| !c.is_live()); // stable: running first
    if rows.is_empty() {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            let msg = if !data.is_loaded(Kind::Containers) {
                "Loading…"
            } else if data.containers.is_empty() {
                "No containers yet. Click “Run container” or pull an image first."
            } else {
                "No container matches the filter."
            };
            ui.label(RichText::new(msg).weak());
        });
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for c in rows {
            row(ui, c, effective_state(c, &data.paused), out);
        }
    });
}

fn row(ui: &mut Ui, c: &Container, state: State, out: &mut Vec<Cmd>) {
    let color = theme::state_color(state, c.health.as_deref());
    let id = c.id.as_str();
    let name = c.label().to_string();
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("●").color(color).size(16.0));
            ui.label(RichText::new(&name).strong().size(15.0));
            ui.label(RichText::new(&c.image).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| theme::pill(ui, &status_text(c, state), color));
        });
        ui.horizontal_wrapped(|ui| {
            let ip = if c.ip.is_empty() { "-" } else { &c.ip };
            let ports = if c.ports.is_empty() { "-" } else { &c.ports };
            ui.label(RichText::new(format!("IP {ip}   ·   Ports {ports}   ·   {}", &c.id[..c.id.len().min(12)])).weak().small());
            if !c.command.is_empty() {
                ui.label(RichText::new(format!("·  {}", c.command)).weak().small().monospace());
            }
        });
        ui.horizontal(|ui| {
            let live = c.is_live();
            let t = |label: &str, args: &[&str]| Cmd::Task(Task::new(format!("{label} {name}"), args));
            if !live && theme::icon_button(ui, "Start", "Start") {
                out.push(t("Start", &["start", id]));
            }
            if state == State::Paused && theme::icon_button(ui, "Resume", "Unpause") {
                out.push(t("Unpause", &["unpause", id]));
            }
            if state == State::Running && theme::icon_button(ui, "Pause", "Pause") {
                out.push(t("Pause", &["pause", id]));
            }
            if live && theme::icon_button(ui, "Stop", "Stop") {
                out.push(t("Stop", &["stop", id]));
            }
            if theme::icon_button(ui, "Restart", "Restart") {
                out.push(t("Restart", &["restart", id]));
            }
            if theme::icon_button(ui, "Logs", "View logs") {
                out.push(Cmd::Logs { id: id.into(), name: name.clone() });
            }
            if live && theme::icon_button(ui, "Stats", "Live CPU / memory") {
                out.push(Cmd::Stats { id: id.into(), name: name.clone() });
            }
            if theme::icon_button(ui, "Shell", "Copy the docker exec -it command") {
                out.push(Cmd::Copy(format!("docker exec -it {name} sh")));
            }
            ui.menu_button("More", |ui| {
                if live && ui.button("☠ Kill").clicked() {
                    out.push(t("Kill", &["kill", id]));
                    ui.close_menu();
                }
                if ui.button("📋 Copy container id").clicked() {
                    out.push(Cmd::Copy(c.id.clone()));
                    ui.close_menu();
                }
                if ui.button(RichText::new("🗑 Remove").color(theme::RED)).clicked() {
                    out.push(Cmd::Confirm(format!("Remove container “{name}”? {}", if live { "It is running and will be force-removed." } else { "" }), Task::new(format!("Remove {name}"), &["rm", "-f", id])));
                    ui.close_menu();
                }
            });
        });
    });
    ui.add_space(2.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c() -> Container {
        Container { id: "abc123".into(), name: "web".into(), image: "nginx".into(), command: String::new(), state: State::Running, health: Some("unhealthy".into()), uptime: Some(125), exit_code: None, restarts: None, ip: "10.0.0.2".into(), ports: "80->80".into() }
    }

    #[test]
    fn filter_matches_any_column() {
        let c = c();
        assert!(matches(&c, ""));
        assert!(matches(&c, "WEB"));
        assert!(matches(&c, "10.0.0"));
        assert!(!matches(&c, "redis"));
    }

    #[test]
    fn status_line() {
        let c = c();
        assert_eq!(status_text(&c, State::Running), "Running 2m 5s · unhealthy");
        assert_eq!(status_text(&c, State::Paused), "Paused 2m 5s");
        let e = Container { exit_code: Some(137), health: None, ..c };
        assert_eq!(status_text(&e, State::Exited), "Exited (137)");
    }
}
