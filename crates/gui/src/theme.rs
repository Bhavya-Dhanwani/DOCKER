//! Look and feel: dark/light visuals, status colours and small reusable widgets.
use crate::parse::State;
use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Ui, vec2};

pub const GREEN: Color32 = Color32::from_rgb(46, 160, 67);
pub const GREY: Color32 = Color32::from_rgb(125, 133, 144);
pub const AMBER: Color32 = Color32::from_rgb(210, 153, 34);
pub const RED: Color32 = Color32::from_rgb(218, 54, 51);
pub const ACCENT: Color32 = Color32::from_rgb(36, 112, 220);

pub fn apply(ctx: &egui::Context, dark: bool) {
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let r = CornerRadius::same(6);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = r;
    }
    v.window_corner_radius = CornerRadius::same(10);
    v.selection.bg_fill = ACCENT;
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 8.0);
        s.spacing.button_padding = vec2(10.0, 5.0);
        s.spacing.interact_size.y = 26.0;
    });
}

pub fn state_color(state: State, health: Option<&str>) -> Color32 {
    if health == Some("unhealthy") {
        return RED;
    }
    match state {
        State::Running => GREEN,
        State::Paused | State::Restarting => AMBER,
        State::Created | State::Exited => GREY,
    }
}

pub fn state_text(state: State) -> &'static str {
    match state {
        State::Running => "Running",
        State::Paused => "Paused",
        State::Restarting => "Restarting",
        State::Created => "Created",
        State::Exited => "Exited",
    }
}

/// Rounded coloured label.
pub fn pill(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::new().fill(color).corner_radius(CornerRadius::same(9)).inner_margin(Margin::symmetric(8, 2)).show(ui, |ui| {
        ui.label(RichText::new(text).color(Color32::WHITE).small().strong());
    });
}

/// A bordered card that adapts to the theme.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let fill = ui.visuals().faint_bg_color;
    let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
    egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Small icon button with a tooltip.
pub fn icon_button(ui: &mut Ui, glyph: &str, tip: &str) -> bool {
    ui.add(egui::Button::new(glyph).min_size(vec2(30.0, 26.0))).on_hover_text(tip).clicked()
}

/// Primary (filled) button.
pub fn primary_button(ui: &mut Ui, text: &str) -> bool {
    ui.add(egui::Button::new(RichText::new(text).color(Color32::WHITE)).fill(ACCENT)).clicked()
}

/// Red button for destructive actions.
pub fn danger_button(ui: &mut Ui, text: &str) -> bool {
    ui.add(egui::Button::new(RichText::new(text).color(Color32::WHITE)).fill(RED)).clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_follow_state() {
        assert_eq!(state_color(State::Running, None), GREEN);
        assert_eq!(state_color(State::Running, Some("healthy")), GREEN);
        assert_eq!(state_color(State::Running, Some("unhealthy")), RED);
        assert_eq!(state_color(State::Exited, None), GREY);
        assert_eq!(state_color(State::Paused, None), AMBER);
        assert_eq!(state_color(State::Restarting, None), AMBER);
    }
}
