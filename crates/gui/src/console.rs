//! The two floating viewers: a streamed-output console (logs, pull, build, foreground run) and live stats.
use crate::{format::fmt_bytes, parse::StatRow, theme};
use eframe::egui::{self, RichText};
use std::{
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::Instant,
};

const MAX_LINES: usize = 5000;

pub struct Console {
    pub id: u64,
    pub title: String,
    pub lines: Vec<String>,
    pub running: bool,
    pub ok: Option<bool>,
    pub cancel: Option<Arc<AtomicBool>>,
    /// Some((container id, name)) when this console shows logs, which get a Follow switch.
    pub logs_of: Option<(String, String)>,
    pub follow: bool,
}

impl Console {
    pub fn new(id: u64, title: impl Into<String>, cancel: Arc<AtomicBool>) -> Console {
        Console { id, title: title.into(), lines: vec![], running: true, ok: None, cancel: Some(cancel), logs_of: None, follow: true }
    }

    pub fn push(&mut self, line: String) {
        self.lines.push(line);
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..MAX_LINES / 10);
        }
    }

    pub fn stop(&mut self) {
        if let Some(c) = &self.cancel {
            c.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(PartialEq)]
pub enum ConsoleAction {
    None,
    Close,
    /// the Follow switch was toggled: restart the log stream
    Restream,
}

pub fn show_console(ctx: &egui::Context, c: &mut Console) -> ConsoleAction {
    let mut open = true;
    let mut action = ConsoleAction::None;
    egui::Window::new(&c.title).id(egui::Id::new("console")).open(&mut open).default_size([720.0, 420.0]).resizable(true).show(ctx, |ui| {
        ui.horizontal(|ui| {
            if c.running {
                ui.spinner();
                ui.label("Running…");
                if ui.button("■ Stop").clicked() {
                    c.stop();
                }
            } else {
                match c.ok {
                    Some(true) => ui.label(RichText::new("✔ Finished").color(theme::GREEN)),
                    _ => ui.label(RichText::new("Ended with an error or was stopped").color(theme::RED)),
                };
            }
            if c.logs_of.is_some() && ui.checkbox(&mut c.follow, "Follow").changed() {
                action = ConsoleAction::Restream;
            }
            if ui.button("📋 Copy all").clicked() {
                ui.ctx().copy_text(c.lines.join("\n"));
            }
        });
        ui.separator();
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::ScrollArea::both().auto_shrink([false, false]).stick_to_bottom(c.running && c.follow).show_rows(ui, row_h, c.lines.len(), |ui, range| {
            for l in &c.lines[range] {
                ui.add(egui::Label::new(RichText::new(l).monospace()).extend());
            }
        });
    });
    if !open { ConsoleAction::Close } else { action }
}

pub struct StatsView {
    pub id: String,
    pub name: String,
    pub last: Option<StatRow>,
    pub err: Option<String>,
    pub pending: bool,
    pub polled: Option<Instant>,
}

/// Returns false when the window was closed.
pub fn show_stats(ctx: &egui::Context, s: &StatsView) -> bool {
    let mut open = true;
    egui::Window::new(format!("Stats: {}", s.name)).id(egui::Id::new("stats")).open(&mut open).default_width(380.0).show(ctx, |ui| {
        if let Some(e) = &s.err {
            ui.colored_label(theme::RED, e);
        }
        match &s.last {
            None if s.err.is_none() => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Sampling…");
                });
            }
            None => {}
            Some(r) => {
                ui.label(format!("CPU  {:.1}%", r.cpu));
                ui.add(egui::ProgressBar::new((r.cpu / 100.0).clamp(0.0, 1.0)));
                let mem = match r.mem_limit {
                    Some(l) => format!("{} / {}", fmt_bytes(r.mem_used), fmt_bytes(l)),
                    None => fmt_bytes(r.mem_used),
                };
                ui.label(format!("Memory  {mem}"));
                ui.add(egui::ProgressBar::new(r.mem_pct.map_or(0.0, |p| (p / 100.0).clamp(0.0, 1.0))));
                ui.label(format!("Processes  {}", r.pids));
            }
        }
    });
    open
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_caps_lines() {
        let mut c = Console::new(1, "t", Arc::new(AtomicBool::new(false)));
        for i in 0..MAX_LINES + 10 {
            c.push(i.to_string());
        }
        assert!(c.lines.len() <= MAX_LINES);
        assert_eq!(c.lines.last().unwrap(), &(MAX_LINES + 9).to_string());
    }

    #[test]
    fn dropping_cancels_stream() {
        let flag = Arc::new(AtomicBool::new(false));
        drop(Console::new(1, "t", flag.clone()));
        assert!(flag.load(Ordering::Relaxed));
    }
}
