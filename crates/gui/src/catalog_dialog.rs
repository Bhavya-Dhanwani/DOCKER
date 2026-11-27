//! "Add image" dialog: your own images, the built-in catalog and Docker Hub search; tick images, pull them.
use crate::{
    catalog, theme,
    worker::{self, Res},
};
use eframe::egui::{self, RichText};
use std::{
    collections::HashSet,
    sync::mpsc::{Receiver, channel},
    thread,
};

#[derive(Default)]
pub struct CatalogDialog {
    pub query: String,
    pub category: Option<&'static str>,
    /// ticked references (name:tag)
    pub selected: HashSet<String>,
    /// extra names typed by hand, separated by spaces/commas/newlines
    pub typed: String,
    pub error: Option<String>,
    /// your own list (`docker catalog`): image, description
    pub mine: Vec<(String, String)>,
    hits: Vec<catalog::Hit>,
    searching: Option<Receiver<Res<String>>>,
    server: String,
    user: String,
    token: String,
    login: Option<Receiver<Res<()>>>,
    /// outcome of the last registry login or Hub search
    note: Option<Res<String>>,
}

pub enum CatalogOutcome {
    Open,
    Cancel,
    Pull(Vec<String>),
}

/// Runs `f` off the UI thread; the window repaints when the answer is ready.
fn background<T: Send + 'static>(ctx: &egui::Context, f: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    thread::spawn(move || {
        let _ = tx.send(f());
        ctx.request_repaint();
    });
    rx
}

impl CatalogDialog {
    pub fn open() -> Self {
        let mut d = Self::default();
        d.reload_mine();
        d
    }

    fn reload_mine(&mut self) {
        // a small local file: quick enough for the UI thread
        match worker::run_cli(&worker::args(&["catalog"])) {
            Ok(o) => self.mine = catalog::parse_mine(&o),
            Err(e) => self.error = Some(e),
        }
    }

    fn change_mine(&mut self, args: &[&str]) {
        if let Err(e) = worker::run_cli(&worker::args(args)) {
            self.error = Some(e);
        }
        self.reload_mine();
    }

    fn poll(&mut self) {
        if let Some(r) = self.searching.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.searching = None;
            match r {
                Ok(o) => {
                    self.hits = catalog::parse_search(&o);
                    self.note = self.hits.is_empty().then(|| Ok("Docker Hub found nothing".to_string()));
                }
                Err(e) => self.note = Some(Err(e)),
            }
        }
        if let Some(r) = self.login.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.login = None;
            self.note = Some(r.map(|()| "Registry connected. Type a full image name above to pull it.".to_string()));
        }
    }

    fn tick(&mut self, ui: &mut egui::Ui, reference: &str) {
        let mut on = self.selected.contains(reference);
        if ui.checkbox(&mut on, "").changed() {
            if on { self.selected.insert(reference.to_string()) } else { self.selected.remove(reference) };
        }
        ui.label(RichText::new(reference).strong());
    }

    /// Everything to pull: ticked catalog images (in catalog order) then typed names, validated, no duplicates.
    pub fn targets(&self) -> Result<Vec<String>, String> {
        let mut v: Vec<String> = catalog::CATALOG.iter().map(|c| c.reference()).filter(|r| self.selected.contains(r)).collect();
        // ticked in "My images" or in the Hub results
        let mut others: Vec<String> = self.selected.iter().filter(|r| !v.contains(r)).cloned().collect();
        others.sort();
        v.extend(others);
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
    d.poll();
    egui::Window::new("Add image").open(&mut open).collapsible(false).resizable(true).default_size([640.0, 520.0]).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut d.query).hint_text("🔍 Search images").desired_width(240.0));
            egui::ComboBox::from_id_salt("cat").selected_text(d.category.unwrap_or("All categories")).show_ui(ui, |ui| {
                ui.selectable_value(&mut d.category, None, "All categories");
                for c in catalog::CATEGORIES {
                    ui.selectable_value(&mut d.category, Some(c), c);
                }
            });
            let term = d.query.trim().to_string();
            if ui.add_enabled(!term.is_empty() && d.searching.is_none(), egui::Button::new("🌐 Search Docker Hub")).clicked() {
                d.note = None;
                d.searching = Some(background(ui.ctx(), move || worker::run_cli(&worker::args(&["search", &term]))));
            }
            if d.searching.is_some() {
                ui.spinner();
            }
            ui.label(RichText::new(format!("{} ticked", d.selected.len())).weak());
        });
        let found = catalog::search(&d.query, d.category);
        egui::ScrollArea::vertical().max_height(340.0).auto_shrink([false, false]).show(ui, |ui| {
            if !d.hits.is_empty() {
                let hits = std::mem::take(&mut d.hits);
                let mut hide = false;
                ui.horizontal(|ui| {
                    ui.heading("Docker Hub");
                    hide = ui.small_button("Hide").clicked();
                });
                for h in &hits {
                    ui.horizontal(|ui| {
                        d.tick(ui, &format!("{}:latest", h.name));
                        if h.official {
                            theme::pill(ui, "official", theme::GREEN);
                        }
                    });
                    ui.label(RichText::new(format!("    {}", h.desc)).weak().small());
                }
                if !hide {
                    d.hits = hits;
                }
                ui.separator();
            }
            let q = d.query.trim().to_lowercase();
            let mine: Vec<(String, String)> = d.mine.iter().filter(|(i, t)| d.category.is_none() && (i.to_lowercase().contains(&q) || t.to_lowercase().contains(&q))).cloned().collect();
            if !mine.is_empty() {
                ui.heading("My images");
                for (image, desc) in &mine {
                    ui.horizontal(|ui| {
                        d.tick(ui, image);
                        if ui.small_button("Remove from list").clicked() {
                            d.selected.remove(image);
                            d.change_mine(&["catalog", "rm", image]);
                        }
                    });
                    if !desc.is_empty() {
                        ui.label(RichText::new(format!("    {desc}")).weak().small());
                    }
                }
                ui.separator();
            }
            if found.is_empty() {
                ui.label(RichText::new("No catalog image matches. Search Docker Hub, or type a name below.").weak());
            }
            for e in found {
                let r = e.reference();
                let have = local.contains(&r);
                ui.horizontal(|ui| {
                    d.tick(ui, &r);
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
            if ui.button("★ Save to my list").on_hover_text("Keep these names in “My images”").clicked() {
                let names: Vec<String> = d.typed.split([' ', ',', '\n', '\t']).filter(|t| !t.is_empty()).map(String::from).collect();
                d.error = names.is_empty().then(|| "Type an image name to save, such as ghcr.io/org/app:1".to_string());
                for n in names {
                    d.change_mine(&["catalog", "add", &n]);
                }
            }
        });
        ui.collapsing("Connect a registry (private images)", |ui| {
            ui.add(egui::TextEdit::singleline(&mut d.server).hint_text("server, e.g. ghcr.io (empty = Docker Hub)").desired_width(380.0));
            ui.add(egui::TextEdit::singleline(&mut d.user).hint_text("username").desired_width(380.0));
            ui.add(egui::TextEdit::singleline(&mut d.token).password(true).hint_text("password or access token").desired_width(380.0));
            let ready = !d.user.trim().is_empty() && !d.token.is_empty() && d.login.is_none();
            if ui.add_enabled(ready, egui::Button::new("Connect")).clicked() {
                let mut a = worker::args(&["login", "-u", d.user.trim(), "--password-stdin"]);
                if !d.server.trim().is_empty() {
                    a.push(d.server.trim().to_string());
                }
                let token = std::mem::take(&mut d.token);
                d.note = None;
                d.login = Some(background(ui.ctx(), move || worker::run_cli_input(&a, &token)));
            }
        });
        match &d.note {
            Some(Ok(m)) => {
                ui.colored_label(theme::GREEN, m);
            }
            Some(Err(m)) => {
                ui.colored_label(theme::RED, m);
            }
            None => {}
        }
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
