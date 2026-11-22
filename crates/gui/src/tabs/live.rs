//! "Live" tab: RAM / disk / CPU of every running container, plus a 2-minute RAM history chart.
use crate::{
    charts::{self, Series},
    format::fmt_bytes,
    state::Data,
    theme,
    usage::{SortKey, Track, sort_tracks, totals},
};
use eframe::egui::{self, RichText, Ui};

pub struct UiState {
    pub sort: SortKey,
    pub desc: bool,
    pub stacked: bool,
}

impl Default for UiState {
    fn default() -> Self {
        UiState { sort: SortKey::Ram, desc: true, stacked: false }
    }
}

fn header(ui: &mut Ui, st: &mut UiState, label: &str, key: SortKey) {
    let arrow = if st.sort == key { if st.desc { " ↓" } else { " ↑" } } else { "" };
    if ui.add(egui::Label::new(RichText::new(format!("{label}{arrow}")).strong()).sense(egui::Sense::click())).clicked() {
        if st.sort == key {
            st.desc = !st.desc;
        } else {
            (st.sort, st.desc) = (key, !matches!(key, SortKey::Name | SortKey::Image));
        }
    }
}

pub fn show(ui: &mut Ui, data: &Data, st: &mut UiState) {
    ui.heading("Live resource usage");
    let u = &data.usage;
    if let Some(e) = u.err.as_ref().filter(|_| data.native.is_none()) {
        ui.colored_label(theme::RED, format!("Engine unavailable, retrying: {e}"));
    }
    let mut rows: Vec<&Track> = u.live().collect();
    if rows.is_empty() {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            let msg = if !u.loaded { "Sampling…" } else if u.err.is_some() && data.native.is_none() { "No data while the engine is unavailable." } else { "No running containers." };
            ui.label(RichText::new(msg).weak());
        });
        return;
    }
    sort_tracks(&mut rows, st.sort, st.desc);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("live").num_columns(6).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
            for (l, k) in [("Name", SortKey::Name), ("Image", SortKey::Image), ("RAM", SortKey::Ram), ("Disk (writable layer)", SortKey::Disk), ("CPU", SortKey::Cpu), ("PIDs", SortKey::Pids)] {
                header(ui, st, l, k);
            }
            ui.end_row();
            for t in &rows {
                let s = &t.last;
                ui.label(RichText::new(&s.name).strong());
                ui.label(RichText::new(&s.image).weak());
                ui.horizontal(|ui| {
                    charts::bar(ui, t.pct() / 100.0, charts::level_color(t.pct()), 110.0);
                    ui.label(charts::ram_text(s.mem_bytes, s.mem_limit, t.pct()));
                });
                ui.label(fmt_bytes(s.disk_bytes));
                ui.horizontal(|ui| {
                    charts::bar(ui, s.cpu_percent / 100.0, theme::ACCENT, 70.0);
                    ui.label(format!("{:.1} %", s.cpu_percent));
                });
                ui.label(s.pids.to_string());
                ui.end_row();
            }
            let tot = totals(rows.iter().copied());
            ui.label(RichText::new("Total").strong());
            ui.label(format!("{} running", rows.len()));
            ui.label(RichText::new(fmt_bytes(tot.mem)).strong());
            ui.label(RichText::new(fmt_bytes(tot.disk)).strong());
            ui.label(RichText::new(format!("{:.1} %", tot.cpu)).strong());
            ui.label("");
            ui.end_row();
        });
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("RAM history (last ~2 minutes)").strong());
            ui.checkbox(&mut st.stacked, "Stacked");
        });
        let mut by_name = rows.clone();
        by_name.sort_by(|a, b| a.last.name.cmp(&b.last.name)); // stable colours whatever the table sort
        let series: Vec<Series> = by_name.iter().map(|t| Series { name: t.last.name.clone(), values: t.ring.iter().map(|&b| b as f64).collect() }).collect();
        charts::history_chart(ui, &series, st.stacked, 240.0);
        ui.label(RichText::new("Container writes (the writable layer) live in RAM-backed tmpfs, so disk usage also counts toward RAM.").weak().small());
    });
}
