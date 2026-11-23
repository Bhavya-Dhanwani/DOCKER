//! Desktop app for Sasta Docker: containers, images, volumes and networks.
//! It talks to the engine only through the sibling `docker` CLI, on background threads.
mod app;
mod catalog;
mod catalog_dialog;
mod charts;
mod console;
mod dialogs;
mod engine;
mod format;
mod group_dialog;
mod groups;
mod parse;
mod spec;
mod state;
mod tabs;
mod theme;
mod usage;
mod worker;

use eframe::egui;

/// the logo as 128x128 RGBA: a card for the window icon, a white silhouette (tinted to the theme) for the sidebar
pub struct Logo;
pub const LOGO: Logo = Logo;
const LOGO_RGBA: &[u8] = include_bytes!("icon.rgba");
const SIDEBAR_RGBA: &[u8] = include_bytes!("sidebar.rgba");
impl Logo {
    pub fn to_color_image(&self) -> egui::ColorImage {
        egui::ColorImage::from_rgba_unmultiplied([256, 256], SIDEBAR_RGBA)
    }
}

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1040.0, 680.0]).with_min_inner_size([760.0, 440.0]).with_title("Sasta Docker").with_icon(egui::IconData { rgba: LOGO_RGBA.to_vec(), width: 128, height: 128 }),
        ..Default::default()
    };
    eframe::run_native("docker-gui", opts, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
