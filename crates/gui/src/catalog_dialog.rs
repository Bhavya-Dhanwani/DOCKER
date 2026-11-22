//! "Add image" dialog: browse/search the static catalog, tick images, pull them.
use crate::{catalog, theme};
use eframe::egui::{self, RichText};
use std::collections::HashSet;

#[derive(Default)]
pub struct CatalogDialog {
    pub query: String,
    pub category: Option<&'static str>,
    /// ticked references (name:tag)
    pub selected: HashSet<String>,
    /// extra names typed by hand, separated by spaces/commas/newlines
    pub typed: String,
    pub error: Option<String>,
}

pub enum CatalogOutcome {
    Open,
    Cancel,
    Pull(Vec<String>),
}

impl CatalogDialog {
    /// Everything to pull: ticked catalog images (in catalog order) then typed names, validated, no duplicates.
    pub fn targets(&self) -> Result<Vec<String>, String> {
        let mut v: Vec<String> = catalog::CATALOG.iter().map(|c| c.reference()).filter(|r| self.selected.contains(r)).collect();
        for t in self.typed.split([' ', ',', '\n', '\t']).map(str::trim).filter(|t| !t.is_empty()) {
            catalog::pull_args(t)?;
            if !v.iter().any(|x| x == t) {
                v.push(t.to_string());
            }
        }
        if v.is_empty() { Err("Tick at least one image or type a name such as redis:7".into()) } else { Ok(v) }
    }
}

pub fn show(ctx: &egui::Context, d: &mut CatalogDialog, local: &[String]) -> CatalogOutcome {
    let mut open = true;
    let mut out = CatalogOutcome::Open;
    egui::Window::new("Add image").open(&mut open).collapsible(false).resizable(true).default_size([640.0, 520.0]).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut d.query).hint_text("🔍 Search images").desired_width(240.0));
            egui::ComboBox::from_id_salt("cat").selected_text(d.category.unwrap_or("All categories")).show_ui(ui, |ui| {
                ui.selectable_value(&mut d.category, None, "All categories");
                for c in catalog::CATEGORIES {
                    ui.selectable_value(&mut d.category, Some(c), c);
                }
            });
            ui.label(RichText::new(format!("{} ticked", d.selected.len())).weak());
        });
        let found = catalog::search(&d.query, d.category);
        egui::ScrollArea::vertical().max_height(340.0).auto_shrink([false, false]).show(ui, |ui| {
            if found.is_empty() {
                ui.label(RichText::new("No catalog image matches. You can still type a name below.").weak());
            }
            for e in found {
                let r = e.reference();
                let have = local.contains(&r);
                ui.horizontal(|ui| {
                    let mut on = d.selected.contains(&r);
                    if ui.checkbox(&mut on, "").changed() {
                        if on { d.selected.insert(r.clone()) } else { d.selected.remove(&r) };
                    }
                    ui.label(RichText::new(&r).strong());
                    if e.tested {
                        theme::pill(ui, "tested", theme::GREEN);
                    } else {
                        ui.label(RichText::new("should work (untested)").color(theme::AMBER).small());
                    }
                    if have {
                        ui.label(RichText::new("already pulled").weak().small());
                    }
                });
                let mut extra = vec![e.category.to_string()];
                if !e.ports.is_empty() {
                    extra.push(format!("port {}", e.ports));
                }
                if !e.env.is_empty() {
                    extra.push(format!("needs {}", e.env.iter().map(|v| v.split('=').next().unwrap_or("")).collect::<Vec<_>>().join(", ")));
                }
                ui.label(RichText::new(format!("    {}  ·  {}", e.desc, extra.join("  ·  "))).weak().small());
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Other:");
            ui.add(egui::TextEdit::singleline(&mut d.typed).hint_text("any name, e.g. redis:7 ghcr.io/org/app:1").desired_width(380.0));
        });
        if let Some(e) = &d.error {
            ui.colored_label(theme::RED, e);
        }
        ui.horizontal(|ui| {
            if theme::primary_button(ui, "⬇ Pull selected") {
                match d.targets() {
                    Ok(t) => out = CatalogOutcome::Pull(t),
                    Err(e) => d.error = Some(e),
                }
            }
            if ui.button("Close").clicked() {
                out = CatalogOutcome::Cancel;
            }
        });
    });
    if open { out } else { CatalogOutcome::Cancel }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_merge_ticked_and_typed() {
        let mut d = CatalogDialog::default();
        assert!(d.targets().is_err());
        d.selected.insert("redis:alpine".into());
        d.selected.insert("alpine:latest".into());
        d.typed = "redis:7, redis:alpine\nghcr.io/o/a:1".into();
        assert_eq!(d.targets().unwrap(), ["alpine:latest", "redis:alpine", "redis:7", "ghcr.io/o/a:1"]);
    }
}
