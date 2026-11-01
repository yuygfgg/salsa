use std::path::{Path, PathBuf};

use salsa_gui::{app, settings};

const PLUGIN_LIBRARIES: [&str; 6] = [
    "libgstsalsaeac3joc.dylib",
    "libgstsalsaeac3joc.so",
    "libgstsalsaeac3joc.dll",
    "libgstsalsaobjectaudio.dylib",
    "libgstsalsaobjectaudio.so",
    "libgstsalsaobjectaudio.dll",
];

fn main() -> eframe::Result<()> {
    configure_environment();
    if let Err(error) = gstreamer::init() {
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

fn configure_environment() {
    if std::env::var_os("GST_PLUGIN_PATH").is_none()
        && let Some(directory) = find_plugin_directory()
    {
        // SAFETY: called before gst::init and before any GStreamer thread exists.
        unsafe {
            std::env::set_var("GST_PLUGIN_PATH", directory);
        }
    }
    #[cfg(target_os = "macos")]
    if std::env::var_os("GST_REGISTRY_FORK").is_none() {
        // SAFETY: called before gst::init and before any GStreamer thread exists.
        unsafe {
            std::env::set_var("GST_REGISTRY_FORK", "no");
        }
    }
}

fn find_plugin_directory() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(directory) = std::env::var_os("SALSA_PLUGIN_DIR") {
        candidates.push(PathBuf::from(directory));
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.to_path_buf());
        candidates.push(directory.join("lib").join("gstreamer-1.0"));
        if let Some(contents) = directory.parent() {
            candidates.push(contents.join("Frameworks").join("gstreamer-1.0"));
            candidates.push(contents.join("Resources").join("gstreamer-1.0"));
        }
        add_ancestors(directory, &mut candidates);
    }
    if let Ok(current) = std::env::current_dir() {
        add_ancestors(&current, &mut candidates);
    }
    candidates
        .into_iter()
        .find(|directory| has_plugin(directory))
}

fn add_ancestors(start: &Path, candidates: &mut Vec<PathBuf>) {
    let mut current = Some(start);
    while let Some(directory) = current {
        candidates.push(directory.join("target").join("release"));
        candidates.push(directory.join("target").join("debug"));
        current = directory.parent();
    }
}

fn has_plugin(directory: &Path) -> bool {
    PLUGIN_LIBRARIES
        .iter()
        .any(|name| directory.join(name).exists())
}
