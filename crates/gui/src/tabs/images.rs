use crate::{
    format::fmt_bytes,
    state::{Cmd, Data, Kind, Task},
    theme,
};
use eframe::egui::{self, RichText, Ui};
use std::collections::HashSet;

#[derive(Default)]
pub struct UiState {
    pub pull: String,
    /// images ticked for "Run selected together"
    pub selected: HashSet<String>,
}

/// Primary button that is disabled (with a tooltip) when the Linux engine is missing.
fn engine_button(ui: &mut Ui, text: &str, native: bool) -> bool {
    let b = egui::Button::new(RichText::new(text).color(egui::Color32::WHITE)).fill(theme::ACCENT);
    ui.add_enabled(!native, b).on_disabled_hover_text(crate::engine::WHY_DISABLED).clicked()
}

pub fn show(ui: &mut Ui, data: &Data, st: &mut UiState, out: &mut Vec<Cmd>) {
    let native = data.native.is_some();
    ui.horizontal(|ui| {
        ui.heading("Images");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("🧹 Prune unused").clicked() {
                out.push(Cmd::Confirm("Delete unused image layers and build leftovers?".into(), Task::new("Prune images", &["prune"])));
            }
            if ui.button("🔨 Build…").clicked() {
                out.push(Cmd::BuildDialog);
            }
            if ui.button("📋 Export image list").on_hover_text("Copy the catalog (tested / untested) as Markdown").clicked() {
                out.push(Cmd::CopyNote(crate::catalog::to_markdown(), "Image catalog copied as Markdown".into()));
            }
            if engine_button(ui, "＋ Add image", native) {
                out.push(Cmd::CatalogDialog);
            }
        });
    });
    ui.horizontal(|ui| {
        let r = ui.add(egui::TextEdit::singleline(&mut st.pull).hint_text("Image to pull, e.g. alpine:latest or ghcr.io/org/app:tag").desired_width(420.0));
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (engine_button(ui, "⬇ Pull", native) || (enter && !native)) && !st.pull.trim().is_empty() {
            out.push(Cmd::Pull(st.pull.trim().to_string()));
            st.pull.clear();
        }
    });
    // images may disappear while ticked
    st.selected.retain(|n| data.images.iter().any(|i| i.name == *n));
    ui.horizontal(|ui| {
        let n = st.selected.len();
        let label = if n == 0 { "▶ Run selected together".to_string() } else { format!("▶ Run {n} selected together") };
        let tip = if native { crate::engine::WHY_DISABLED } else { "Start the ticked images as one group on a shared network" };
        if ui.add_enabled(n >= 1 && !native, egui::Button::new(label)).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
            let picked = data.images.iter().filter(|i| st.selected.contains(&i.name)).map(|i| i.name.clone()).collect();
            out.push(Cmd::GroupDialog(picked));
        }
        if n > 0 && ui.small_button("Clear selection").clicked() {
            st.selected.clear();
        }
    });
    ui.separator();

    if data.images.is_empty() {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| ui.label(RichText::new(if data.is_loaded(Kind::Images) { if native { "No images (native Windows mode has no image store)." } else { "No images. Pull one above." } } else { "Loading…" }).weak()));
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for im in &data.images {
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut on = st.selected.contains(&im.name);
                    if ui.checkbox(&mut on, "").changed() {
                        if on { st.selected.insert(im.name.clone()) } else { st.selected.remove(&im.name) };
                    }
                    ui.label(RichText::new("📦").size(18.0));
                    ui.label(RichText::new(&im.name).strong().size(15.0));
                    ui.label(RichText::new(format!("{}  ·  {} layer{}", fmt_bytes(im.size), im.layers, if im.layers == 1 { "" } else { "s" })).weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::icon_button(ui, "Remove", "Remove image") {
                            out.push(Cmd::Confirm(format!("Remove image “{}”?", im.name), Task::new(format!("Remove image {}", im.name), &["rmi", &im.name])));
                        }
                        if ui.add_enabled(!native, egui::Button::new("▶ Run…")).on_disabled_hover_text(crate::engine::WHY_DISABLED).clicked() {
                            out.push(Cmd::RunDialog(im.name.clone()));
                        }
                    });
                });
            });
        }
    });
}
