//! Desktop app for Sasta Docker: containers, images, volumes and networks.
//! It talks to the engine only through the sibling `docker` CLI, on background threads.
mod app;
mod console;
mod dialogs;
mod format;
mod parse;
mod spec;
mod state;
mod tabs;
mod theme;
mod worker;

use eframe::egui;

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1040.0, 680.0]).with_min_inner_size([760.0, 440.0]).with_title("Sasta Docker"),
        ..Default::default()
    };
    eframe::run_native("docker-gui", opts, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
