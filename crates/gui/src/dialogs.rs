//! Modal-style windows: run container, build image, confirm.
use crate::{
    spec::{BuildSpec, FolderSpec, Restart, RunSpec},
    theme,
    worker::Worker,
};
use eframe::egui::{self, RichText, Ui};

pub enum Dialog {
    Run(Box<RunDialog>),
    Build(BuildDialog),
    Group(crate::group_dialog::GroupDialog),
    Catalog(crate::catalog_dialog::CatalogDialog),
}

#[derive(Default)]
pub struct RunDialog {
    pub spec: RunSpec,
    pub folder_mode: bool,
    pub folder: FolderSpec,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct BuildDialog {
    pub spec: BuildSpec,
    pub error: Option<String>,
}

pub enum Outcome {
    Open,
    Cancel,
    /// Start streaming this CLI call under this title; `detach` runs it as a quiet task instead.
    Submit { title: String, args: Vec<String>, detach: bool },
}

impl RunDialog {
    pub fn for_image(image: &str) -> Self {
        RunDialog { spec: RunSpec { image: image.into(), ..Default::default() }, folder_mode: false, folder: FolderSpec::default(), error: None }
    }
}

/// Folder name of a path, as a default image tag.
pub fn default_tag(folder: &str) -> String {
    let leaf = folder.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("");
    let tag: String = leaf.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || "_.-".contains(c) { c } else { '-' }).collect();
    if tag.is_empty() { String::new() } else { format!("{tag}:latest") }
}

fn field(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.label(label);
    add(ui);
    ui.end_row();
}

/// Some(true) = primary button, Some(false) = cancel.
fn footer(ui: &mut Ui, error: &Option<String>, ok: &str) -> Option<bool> {
    if let Some(e) = error {
        ui.colored_label(theme::RED, e);
    }
    let mut out = None;
    ui.horizontal(|ui| {
        if theme::primary_button(ui, ok) {
            out = Some(true);
        }
        if ui.button("Cancel").clicked() {
            out = Some(false);
        }
    });
    out
}

pub fn show_run(ctx: &egui::Context, d: &mut RunDialog, w: &Worker, native: bool) -> Outcome {
    if native {
        d.folder_mode = true; // images cannot run without the Linux engine
    }
    let mut open = true;
    let mut out = Outcome::Open;
    egui::Window::new("Run container").open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        ui.add_enabled(!native, egui::Checkbox::new(&mut d.folder_mode, "Run a Windows folder (native program, no image)")).on_disabled_hover_text(crate::engine::WHY_DISABLED);
        if d.folder_mode {
            let f = &mut d.folder;
            egui::Grid::new("runf").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                field(ui, "Folder", |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut f.folder).hint_text("C:\\path\\to\\app").desired_width(280.0));
                        if cfg!(windows) && ui.button("📁 Browse…").clicked() {
                            w.pick_folder();
                        }
                    });
                });
                field(ui, "Program", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.program).hint_text("node.exe").desired_width(340.0));
                });
                field(ui, "Arguments", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.args).hint_text("optional, e.g. server.js --port 8080").desired_width(340.0));
                });
                field(ui, "Memory limit (MB)", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.mem_mb).hint_text("no limit").desired_width(120.0));
                });
                field(ui, "CPU limit (%)", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.cpu).hint_text("no limit").desired_width(120.0));
                });
            });
            ui.add_space(4.0);
            match footer(ui, &d.error, "▶ Run") {
                Some(true) => match d.folder.args() {
                    Ok(args) => out = Outcome::Submit { title: format!("Run {}", d.folder.program.trim()), args, detach: false },
                    Err(e) => d.error = Some(e),
                },
                Some(false) => out = Outcome::Cancel,
                None => {}
            }
            return;
        }
        let s = &mut d.spec;
        egui::Grid::new("run").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            field(ui, "Image", |ui| {
                ui.add(egui::TextEdit::singleline(&mut s.image).hint_text("alpine:latest").desired_width(340.0));
            });
            field(ui, "Name", |ui| {
                ui.add(egui::TextEdit::singleline(&mut s.name).hint_text("optional").desired_width(340.0));
            });
            field(ui, "Command", |ui| {
                ui.add(egui::TextEdit::singleline(&mut s.command).hint_text("optional, e.g. sleep 60").desired_width(340.0));
            });
            field(ui, "Ports", |ui| {
                ui.add(egui::TextEdit::multiline(&mut s.ports).hint_text("host:container, one per line (8080:80)").desired_rows(2).desired_width(340.0));
            });
            field(ui, "Environment", |ui| {
                ui.add(egui::TextEdit::multiline(&mut s.env).hint_text("KEY=value, one per line").desired_rows(2).desired_width(340.0));
            });
            field(ui, "Volumes", |ui| {
                ui.add(egui::TextEdit::multiline(&mut s.volumes).hint_text("volume-or-path:/in/container[:ro], one per line").desired_rows(2).desired_width(340.0));
            });
            field(ui, "Memory limit (MB)", |ui| {
                ui.add(egui::TextEdit::singleline(&mut s.mem_mb).hint_text("no limit").desired_width(120.0));
            });
            field(ui, "Restart policy", |ui| {
                egui::ComboBox::from_id_salt("restart").selected_text(s.restart.flag()).show_ui(ui, |ui| {
                    for r in Restart::ALL {
                        ui.selectable_value(&mut s.restart, r, r.flag());
                    }
                });
            });
            field(ui, "", |ui| {
                ui.checkbox(&mut s.detach, "Run in the background (detach)");
            });
        });
        ui.add_space(4.0);
        match footer(ui, &d.error, "▶ Run") {
            Some(true) => match d.spec.args() {
                Ok(args) => out = Outcome::Submit { title: format!("Run {}", d.spec.image.trim()), args, detach: d.spec.detach },
                Err(e) => d.error = Some(e),
            },
            Some(false) => out = Outcome::Cancel,
            None => {}
        }
    });
    if open { out } else { Outcome::Cancel }
}

pub fn show_build(ctx: &egui::Context, d: &mut BuildDialog, w: &Worker) -> Outcome {
    let mut open = true;
    let mut out = Outcome::Open;
    egui::Window::new("Build image").open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        let s = &mut d.spec;
        egui::Grid::new("build").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            field(ui, "Folder", |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut s.context).hint_text("folder with a Dockerfile").desired_width(280.0));
                    if cfg!(windows) && ui.button("📁 Browse…").clicked() {
                        w.pick_folder();
                    }
                });
            });
            field(ui, "Tag", |ui| {
                ui.add(egui::TextEdit::singleline(&mut s.tag).hint_text("myapp:latest").desired_width(340.0));
            });
            field(ui, "Dockerfile", |ui| {
                ui.add(egui::TextEdit::singleline(&mut s.dockerfile).hint_text("optional, default: Dockerfile").desired_width(340.0));
            });
            field(ui, "", |ui| {
                ui.checkbox(&mut s.no_cache, "Do not use the build cache");
            });
        });
        ui.add_space(4.0);
        match footer(ui, &d.error, "🔨 Build") {
            Some(true) => match d.spec.args() {
                Ok(args) => out = Outcome::Submit { title: format!("Build {}", d.spec.tag.trim()), args, detach: false },
                Err(e) => d.error = Some(e),
            },
            Some(false) => out = Outcome::Cancel,
            None => {}
        }
    });
    if open { out } else { Outcome::Cancel }
}

/// Some(true) = confirmed, Some(false) = cancelled.
pub fn show_confirm(ctx: &egui::Context, message: &str) -> Option<bool> {
    let mut answer = None;
    egui::Window::new("Please confirm").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        ui.label(RichText::new(message).size(14.0));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if theme::danger_button(ui, "Yes, continue") {
                answer = Some(true);
            }
            if ui.button("Cancel").clicked() {
                answer = Some(false);
            }
        });
    });
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_from_folder() {
        assert_eq!(default_tag("C:\\work\\My App\\"), "my-app:latest");
        assert_eq!(default_tag("/srv/web"), "web:latest");
        assert_eq!(default_tag(""), "");
    }
}
