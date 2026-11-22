//! Small painter-based widgets: usage bars, sparklines and the history chart (no extra dependencies).
use crate::{
    format::fmt_bytes,
    theme,
    usage::{Level, level},
};
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};

pub fn level_color(pct: f64) -> Color32 {
    match level(pct) {
        Level::Ok => theme::GREEN,
        Level::Warn => theme::AMBER,
        Level::Hot => theme::RED,
    }
}

/// A horizontal bar filled to `frac` (0..1).
pub fn bar(ui: &mut Ui, frac: f64, color: Color32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 12.0), Sense::hover());
    let track = ui.visuals().widgets.inactive.bg_fill;
    ui.painter().rect_filled(rect, 4.0, track);
    let w = rect.width() * frac.clamp(0.0, 1.0) as f32;
    if w > 0.5 {
        ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(w.max(3.0), rect.height())), 4.0, color);
    }
}

/// "43.3 MB / 200 MB (20.6 %)"
pub fn ram_text(used: u64, limit: u64, pct: f64) -> String {
    if limit > 0 { format!("{} / {} ({pct:.1} %)", fmt_bytes(used), fmt_bytes(limit)) } else { fmt_bytes(used) }
}

pub fn series_color(i: usize) -> Color32 {
    const P: [Color32; 10] = [
        Color32::from_rgb(36, 112, 220),
        Color32::from_rgb(46, 160, 67),
        Color32::from_rgb(210, 153, 34),
        Color32::from_rgb(218, 54, 51),
        Color32::from_rgb(137, 87, 229),
        Color32::from_rgb(0, 160, 176),
        Color32::from_rgb(219, 97, 162),
        Color32::from_rgb(140, 100, 60),
        Color32::from_rgb(120, 130, 140),
        Color32::from_rgb(160, 180, 40),
    ];
    P[i % P.len()]
}

/// Points of a series laid out right-aligned in `rect`: newest sample at the right edge.
pub fn plot_points(rect: Rect, values: &[f64], max: f64, span: usize) -> Vec<Pos2> {
    let max = if max > 0.0 { max } else { 1.0 };
    let step = if span > 1 { rect.width() / (span - 1) as f32 } else { 0.0 };
    let n = values.len();
    values.iter().enumerate().map(|(i, v)| pos2(rect.right() - (n - 1 - i) as f32 * step, rect.bottom() - (*v / max).clamp(0.0, 1.0) as f32 * rect.height())).collect()
}

/// Tiny RAM-over-time line. Scaled to the peak (at least 8 MB) so a spike stands out.
pub fn sparkline(ui: &mut Ui, values: &[f64], size: egui::Vec2, color: Color32) {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
    if values.len() >= 2 {
        let max = values.iter().cloned().fold(8.0 * 1048576.0, f64::max) * 1.15;
        let inner = rect.shrink2(vec2(3.0, 4.0));
        let pts = plot_points(inner, values, max, crate::usage::HISTORY);
        p.add(egui::Shape::line(pts, Stroke::new(1.5_f32, color)));
    }
    resp.on_hover_text("RAM over the last ~2 minutes");
}

/// Cumulative sums, so series can be drawn stacked: out[k][i] = sum of series 0..=k at i.
/// Series are right-aligned (shorter ones start later and count as 0 before that).
pub fn stack(series: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let n = series.iter().map(Vec::len).max().unwrap_or(0);
    let mut acc = vec![0.0; n];
    series
        .iter()
        .map(|s| {
            for (i, v) in s.iter().enumerate() {
                acc[n - s.len() + i] += v;
            }
            acc.clone()
        })
        .collect()
}

pub struct Series {
    pub name: String,
    pub values: Vec<f64>,
}

/// Big RAM history chart with a legend. `stacked` draws cumulative lines.
pub fn history_chart(ui: &mut Ui, series: &[Series], stacked: bool, height: f32) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, height), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 6.0, ui.visuals().extreme_bg_color);
    let plot = Rect::from_min_max(rect.min + vec2(64.0, 10.0), rect.max - vec2(12.0, 22.0));
    let raw: Vec<Vec<f64>> = series.iter().map(|s| s.values.clone()).collect();
    let drawn = if stacked { stack(&raw) } else { raw };
    let max = drawn.iter().flat_map(|v| v.iter().cloned()).fold(1.0, f64::max) * 1.1;
    let grid = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let text = ui.visuals().weak_text_color();
    for k in 0..=4 {
        let y = plot.bottom() - plot.height() * k as f32 / 4.0;
        p.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0_f32, grid));
        p.text(pos2(plot.left() - 6.0, y), egui::Align2::RIGHT_CENTER, fmt_bytes((max * k as f64 / 4.0) as u64), egui::FontId::proportional(11.0), text);
    }
    p.text(pos2(plot.left(), plot.bottom() + 4.0), egui::Align2::LEFT_TOP, "-2 min", egui::FontId::proportional(11.0), text);
    p.text(pos2(plot.right(), plot.bottom() + 4.0), egui::Align2::RIGHT_TOP, "now", egui::FontId::proportional(11.0), text);
    for (i, v) in drawn.iter().enumerate().rev() {
        let pts = plot_points(plot, v, max, crate::usage::HISTORY);
        if pts.len() >= 2 {
            p.add(egui::Shape::line(pts, Stroke::new(2.0_f32, series_color(i))));
        }
    }
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        for (i, s) in series.iter().enumerate() {
            let last = s.values.last().copied().unwrap_or(0.0);
            let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
            ui.painter().rect_filled(r, 3.0, series_color(i));
            ui.label(format!("{}  {}", s.name, fmt_bytes(last as u64)));
            ui.add_space(8.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_by_level() {
        assert_eq!(level_color(10.0), theme::GREEN);
        assert_eq!(level_color(70.0), theme::AMBER);
        assert_eq!(level_color(90.0), theme::RED);
    }

    #[test]
    fn ram_label() {
        assert_eq!(ram_text(43_274_240, 209_715_200, 20.6), "41.3 MB / 200.0 MB (20.6 %)");
        assert_eq!(ram_text(1 << 20, 0, 0.0), "1.0 MB");
    }

    #[test]
    fn stacking_is_cumulative_and_right_aligned() {
        let s = stack(&[vec![1.0, 2.0, 3.0], vec![10.0, 20.0], vec![100.0]]);
        assert_eq!(s[0], [1.0, 2.0, 3.0]);
        assert_eq!(s[1], [1.0, 12.0, 23.0]);
        assert_eq!(s[2], [1.0, 12.0, 123.0]);
    }

    #[test]
    fn points_are_right_aligned() {
        let r = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 50.0));
        let p = plot_points(r, &[0.0, 10.0], 10.0, 11);
        assert_eq!(p[1], pos2(100.0, 0.0));
        assert_eq!(p[0], pos2(90.0, 50.0));
    }
}
