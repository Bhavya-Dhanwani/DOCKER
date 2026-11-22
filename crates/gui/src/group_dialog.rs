//! "Run selected images together" dialog and its progress window.
use crate::{
    groups::{self, GroupRun, MemberSpec, Status},
    spec::Restart,
    theme,
};
use eframe::egui::{self, RichText};

pub struct GroupDialog {
    pub name: String,
    pub members: Vec<MemberSpec>,
    pub error: Option<String>,
}

pub enum GroupOutcome {
    Open,
    Cancel,
    Run { group: String, network: Vec<String>, runs: Vec<Vec<String>>, members: Vec<MemberSpec> },
}

impl GroupDialog {
    pub fn new(images: &[String]) -> GroupDialog {
        let name = groups::default_group_name(images.first().map_or("", String::as_str));
        GroupDialog { members: images.iter().map(|i| MemberSpec::for_image(&name, i)).collect(), name, error: None }
    }

    /// Re-derive default container names when the group name is edited (custom names are kept).
    pub fn rename_group(&mut self, new: &str) {
        for m in &mut self.members {
            if m.name == groups::member_name(&self.name, &m.image) {
                m.name = groups::member_name(new, &m.image);
            }
        }
        self.name = new.to_string();
    }
}

pub fn show(ctx: &egui::Context, d: &mut GroupDialog) -> GroupOutcome {
    let mut open = true;
    let mut out = GroupOutcome::Open;
    egui::Window::new("Run images together as a group").open(&mut open).collapsible(false).resizable(true).default_width(560.0).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        ui.label(RichText::new("Each image becomes its own container, all on one network named after the group. They reach each other by the image's short name (e.g. host “postgres”).").weak());
        ui.horizontal(|ui| {
            ui.label("Group name");
            let mut name = d.name.clone();
            if ui.add(egui::TextEdit::singleline(&mut name).desired_width(220.0)).changed() {
                d.rename_group(&name);
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
            for (i, m) in d.members.iter_mut().enumerate() {
                ui.push_id(i, |ui| {
                    theme::card(ui, |ui| {
                        ui.label(RichText::new(format!("📦 {}", m.image)).strong());
                        egui::Grid::new("m").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                            ui.label("Container name");
                            ui.add(egui::TextEdit::singleline(&mut m.name).desired_width(300.0));
                            ui.end_row();
                            ui.label("Ports");
                            ui.add(egui::TextEdit::multiline(&mut m.ports).hint_text("host:container, one per line (5432:5432)").desired_rows(1).desired_width(300.0));
                            ui.end_row();
                            ui.label("Environment");
                            ui.vertical(|ui| {
                                let rows = if m.env.is_empty() { 1 } else { 2 };
                                ui.add(egui::TextEdit::multiline(&mut m.env).hint_text("KEY=value, one per line").desired_rows(rows).desired_width(300.0));
                                if !m.hint.is_empty() {
                                    ui.label(RichText::new(format!("⚠ {}", m.hint)).color(theme::AMBER).small());
                                }
                            });
                            ui.end_row();
                            ui.label("Memory limit (MB)");
                            ui.add(egui::TextEdit::singleline(&mut m.mem_mb).hint_text("no limit").desired_width(100.0));
                            ui.end_row();
                            ui.label("Restart policy");
                            egui::ComboBox::from_id_salt("rp").selected_text(m.restart.flag()).show_ui(ui, |ui| {
                                for r in Restart::ALL {
                                    ui.selectable_value(&mut m.restart, r, r.flag());
                                }
                            });
                            ui.end_row();
                        });
                    });
                });
            }
        });
        if let Some(e) = &d.error {
            ui.colored_label(theme::RED, e);
        }
        ui.horizontal(|ui| {
            if theme::primary_button(ui, "▶ Run group") {
                match groups::plan(&d.name, &d.members) {
                    Ok((network, runs)) => out = GroupOutcome::Run { group: d.name.trim().to_string(), network, runs, members: d.members.clone() },
                    Err(e) => d.error = Some(e),
                }
            }
            if ui.button("Cancel").clicked() {
                out = GroupOutcome::Cancel;
            }
        });
    });
    if open { out } else { GroupOutcome::Cancel }
}

fn status_label(ui: &mut egui::Ui, s: &Status) {
    match s {
        Status::Pending => ui.label(RichText::new("waiting").weak()),
        Status::Running => ui.label(RichText::new("starting…").color(theme::AMBER)),
        Status::Ok => ui.label(RichText::new("✔ started").color(theme::GREEN)),
        Status::Failed(e) => ui.label(RichText::new(format!("✖ {e}")).color(theme::RED)),
    };
}

/// Progress window for a group run. Returns true while it should stay open.
pub fn show_progress(ctx: &egui::Context, r: &GroupRun) -> bool {
    let mut open = true;
    egui::Window::new(format!("Group “{}”", r.name)).id(egui::Id::new("group-run")).open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        egui::Grid::new("prog").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
            ui.label(format!("Network {}", r.name));
            status_label(ui, &r.network);
            ui.end_row();
            for it in &r.items {
                ui.label(format!("{}  ({})", it.name, it.image));
                status_label(ui, &it.status);
                ui.end_row();
            }
        });
        ui.add_space(4.0);
        if r.done {
            let (ok, bad) = (r.succeeded().len(), r.failed());
            let msg = if bad == 0 { format!("All {ok} containers started.") } else { format!("{ok} started, {bad} failed. The others keep running; fix the failed one and run it again, or remove the group.") };
            ui.label(RichText::new(msg).color(if bad == 0 { theme::GREEN } else { theme::AMBER }));
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Working…");
            });
        }
    });
    open
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renaming_group_updates_default_names_only() {
        let mut d = GroupDialog::new(&["postgres:alpine".into(), "redis".into()]);
        assert_eq!((d.name.as_str(), d.members[0].name.as_str()), ("postgres", "postgres-postgres"));
        d.members[1].name = "my-cache".into();
        d.rename_group("shop");
        assert_eq!((d.members[0].name.as_str(), d.members[1].name.as_str()), ("shop-postgres", "my-cache"));
    }
}
