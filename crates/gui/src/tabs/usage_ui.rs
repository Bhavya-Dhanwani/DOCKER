//! Usage widgets shared by the Containers tab: the RAM block on a container card and the group header.
use crate::{
    charts,
    format::fmt_bytes,
    groups::{self, Group},
    state::{Cmd, Data, Task},
    theme,
    usage::{Track, totals},
};
use eframe::egui::{self, RichText, Ui, vec2};
use std::{collections::HashSet, time::Instant};

/// RAM bar + numbers + sparkline for a running container (or its last known peak when stopped).
pub fn card_block(ui: &mut Ui, t: Option<&Track>, live: bool) {
    let Some(t) = t else { return };
    if !live {
        if t.peak > 0 {
            ui.label(RichText::new(format!("Last known peak RAM: {}", fmt_bytes(t.peak))).weak().small());
        }
        return;
    }
    let s = &t.last;
    let flash = t.flashing(Instant::now());
    egui::Frame::new().fill(if flash { theme::AMBER.gamma_multiply(0.35) } else { egui::Color32::TRANSPARENT }).corner_radius(6).inner_margin(4).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    charts::bar(ui, t.pct() / 100.0, charts::level_color(t.pct()), 220.0);
                    ui.label(RichText::new(charts::ram_text(s.mem_bytes, s.mem_limit, t.pct())).strong());
                    if flash {
                        ui.label(RichText::new("⚡ RAM jumped").color(theme::AMBER).small());
                    }
                });
                ui.label(RichText::new(format!("Disk (writable layer): {}   ·   CPU {:.1} %   ·   PIDs {}   ·   peak RAM {}", fmt_bytes(s.disk_bytes), s.cpu_percent, s.pids, fmt_bytes(t.peak))).small());
            });
            let values: Vec<f64> = t.ring.iter().map(|&b| b as f64).collect();
            charts::sparkline(ui, &values, vec2(180.0, 38.0), charts::level_color(t.pct()));
        });
    });
}

/// Collapsible group header: name, totals and group-level actions. 
pub fn group_header(ui: &mut Ui, g: &Group, data: &Data, collapsed: &mut HashSet<String>, out: &mut Vec<Cmd>) {
    let tot = totals(g.members.iter().filter_map(|m| data.containers.iter().find(|c| c.name == *m)).filter_map(|c| data.usage.get(&c.id)).filter(|t| t.live));
    let is_collapsed = collapsed.contains(&g.name);
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            if ui.button(if is_collapsed { "▸" } else { "▾" }).clicked() {
                if is_collapsed {
                    collapsed.remove(&g.name);
                } else {
                    collapsed.insert(g.name.clone());
                }
            }
            ui.label(RichText::new(format!("🧩 {}", g.name)).strong().size(16.0));
            ui.label(RichText::new(format!("{} container{}   ·   RAM {}   ·   disk {}   ·   network “{}”", g.members.len(), if g.members.len() == 1 { "" } else { "s" }, fmt_bytes(tot.mem), fmt_bytes(tot.disk), g.name)).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("🗑 Remove group").color(theme::RED)).clicked() {
                    out.push(Cmd::Confirm(format!("Remove group “{}”? Its {} container(s) are stopped and deleted, and the network is removed.", g.name, g.members.len()), Task { label: format!("Remove group {}", g.name), steps: groups::remove_steps(g) }));
                }
                if ui.button("■ Stop all").clicked() {
                    out.push(Cmd::Task(Task { label: format!("Stop group {}", g.name), steps: groups::stop_steps(g) }));
                }
                if ui.button("▶ Start all").clicked() {
                    out.push(Cmd::Task(Task { label: format!("Start group {}", g.name), steps: groups::start_steps(g) }));
                }
            });
        });
    });
}
