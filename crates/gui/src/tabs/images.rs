use crate::{
    format::fmt_bytes,
    state::{Cmd, Data, Kind, Task},
    theme,
};
use eframe::egui::{self, RichText, Ui};

#[derive(Default)]
pub struct UiState {
    pub pull: String,
}

pub fn show(ui: &mut Ui, data: &Data, st: &mut UiState, out: &mut Vec<Cmd>) {
    ui.horizontal(|ui| {
        ui.heading("Images");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("🧹 Prune unused").clicked() {
                out.push(Cmd::Confirm("Delete unused image layers and build leftovers?".into(), Task::new("Prune images", &["prune"])));
            }
            if ui.button("🔨 Build…").clicked() {
                out.push(Cmd::BuildDialog);
            }
        });
    });
    ui.horizontal(|ui| {
        let r = ui.add(egui::TextEdit::singleline(&mut st.pull).hint_text("Image to pull, e.g. alpine:latest or ghcr.io/org/app:tag").desired_width(420.0));
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (theme::primary_button(ui, "⬇ Pull") || enter) && !st.pull.trim().is_empty() {
            out.push(Cmd::Pull(st.pull.trim().to_string()));
            st.pull.clear();
        }
    });
    ui.separator();

    if data.images.is_empty() {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| ui.label(RichText::new(if data.is_loaded(Kind::Images) { "No images. Pull one above." } else { "Loading…" }).weak()));
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for im in &data.images {
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("📦").size(18.0));
                    ui.label(RichText::new(&im.name).strong().size(15.0));
                    ui.label(RichText::new(format!("{}  ·  {} layer{}", fmt_bytes(im.size), im.layers, if im.layers == 1 { "" } else { "s" })).weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::icon_button(ui, "Remove", "Remove image") {
                            out.push(Cmd::Confirm(format!("Remove image “{}”?", im.name), Task::new(format!("Remove image {}", im.name), &["rmi", &im.name])));
                        }
                        if ui.button("▶ Run…").clicked() {
                            out.push(Cmd::RunDialog(im.name.clone()));
                        }
                    });
                });
            });
        }
    });
}
