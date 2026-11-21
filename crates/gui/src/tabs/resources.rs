//! Volumes and networks: both are "a name you create and remove".
use crate::{
    format::fmt_bytes,
    state::{Cmd, Data, Kind, Task},
    theme,
};
use eframe::egui::{self, RichText, Ui};

#[derive(Default)]
pub struct UiState {
    pub new_volume: String,
    pub new_network: String,
}

fn create_row(ui: &mut Ui, text: &mut String, hint: &str) -> bool {
    let mut go = false;
    ui.horizontal(|ui| {
        let r = ui.add(egui::TextEdit::singleline(text).hint_text(hint).desired_width(300.0));
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        go = (theme::primary_button(ui, "＋ Create") || enter) && !text.trim().is_empty();
    });
    go
}

fn empty(ui: &mut Ui, loaded: bool, msg: &str) {
    ui.add_space(30.0);
    ui.vertical_centered(|ui| ui.label(RichText::new(if loaded { msg } else { "Loading…" }).weak()));
}

pub fn volumes(ui: &mut Ui, data: &Data, st: &mut UiState, out: &mut Vec<Cmd>) {
    ui.heading("Volumes");
    if create_row(ui, &mut st.new_volume, "New volume name") {
        let n = st.new_volume.trim().to_string();
        out.push(Cmd::Task(Task::new(format!("Create volume {n}"), &["volume", "create", &n])));
        st.new_volume.clear();
    }
    ui.separator();
    if data.volumes.is_empty() {
        return empty(ui, data.is_loaded(Kind::Volumes), "No volumes.");
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for v in &data.volumes {
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("💾").size(18.0));
                    ui.label(RichText::new(&v.name).strong().size(15.0));
                    ui.label(RichText::new(fmt_bytes(v.size)).weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::icon_button(ui, "Remove", "Remove volume") {
                            out.push(Cmd::Confirm(format!("Remove volume “{}” and its data?", v.name), Task::new(format!("Remove volume {}", v.name), &["volume", "rm", &v.name])));
                        }
                    });
                });
            });
        }
    });
}

pub fn networks(ui: &mut Ui, data: &Data, st: &mut UiState, out: &mut Vec<Cmd>) {
    ui.heading("Networks");
    if create_row(ui, &mut st.new_network, "New network name") {
        let n = st.new_network.trim().to_string();
        out.push(Cmd::Task(Task::new(format!("Create network {n}"), &["network", "create", &n])));
        st.new_network.clear();
    }
    ui.separator();
    if data.networks.is_empty() {
        return empty(ui, data.is_loaded(Kind::Networks), "No networks.");
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for n in &data.networks {
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("🌐").size(18.0));
                    ui.label(RichText::new(&n.name).strong().size(15.0));
                    if n.builtin {
                        theme::pill(ui, "default", theme::GREY);
                    } else {
                        ui.label(RichText::new(format!("{} container{}", n.containers, if n.containers == 1 { "" } else { "s" })).weak());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::icon_button(ui, "Remove", "Remove network") {
                                out.push(Cmd::Confirm(format!("Remove network “{}”?", n.name), Task::new(format!("Remove network {}", n.name), &["network", "rm", &n.name])));
                            }
                        });
                    }
                });
            });
        }
    });
}
