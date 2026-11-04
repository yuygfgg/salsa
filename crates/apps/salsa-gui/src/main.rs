use std::path::PathBuf;

use salsa_gui::{app, settings};

fn main() -> eframe::Result<()> {
    if let Err(error) = salsa_gst_registry::init() {
        eprintln!("Salsa Studio requires GStreamer: {error}");
        std::process::exit(1);
    }

    let initial_path = std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .find(|path| !path.to_string_lossy().starts_with('-'));

    let settings = settings::Settings::load();
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("Salsa Studio")
        .with_min_inner_size([980.0, 640.0]);
    if let Some(size) = settings.window_size {
        viewport = viewport.with_inner_size(size);
    }
    if let Some(position) = settings.window_position {
        viewport = viewport.with_position(position);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Salsa Studio",
        options,
        Box::new(move |creation| {
            let mut app = app::SalsaApp::new(creation, settings);
            if let Some(path) = initial_path {
                app.open_path(path);
            }
            Ok(Box::new(app))
        }),
    )
}
