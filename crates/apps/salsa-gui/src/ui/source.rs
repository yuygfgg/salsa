use std::path::{Path, PathBuf};

use eframe::egui;
use egui::{Color32, RichText};
use gstreamer as gst;

use crate::model::SourceInfo;
use crate::settings::Settings;
use crate::ui::format_clock;

pub enum EmptyAction {
    OpenDialog,
    Open(PathBuf),
    CopyDiagnostics,
}

pub fn draw_source(
    ui: &mut egui::Ui,
    source: Option<&SourceInfo>,
    duration: Option<gst::ClockTime>,
    scanning: bool,
) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.strong("Source");
            match source {
                Some(source) => {
                    ui.label(RichText::new(source.file_name()).strong().size(15.0));
                    ui.label(
                        RichText::new(source.path.display().to_string())
                            .weak()
                            .size(10.0),
                    );
                    ui.horizontal(|ui| {
                        ui.label(source.codec.label());
                        ui.weak(format!("{} kHz", source.rate / 1000));
                        ui.weak(format!("{} objects", source.objects));
                    });
                    ui.horizontal(|ui| {
                        let text = duration.map(format_clock).unwrap_or_else(|| {
                            if scanning {
                                "Scanning…".to_string()
                            } else {
                                "--:--".to_string()
                            }
                        });
                        ui.monospace(text);
                        ui.label(
                            RichText::new("Atmos")
                                .color(Color32::from_rgb(130, 210, 160))
                                .strong(),
                        );
                    });
                }
                None => {
                    ui.weak("No source loaded");
                }
            }
        });
}

pub fn draw_empty(
    ui: &mut egui::Ui,
    settings: &Settings,
    plugins_ready: bool,
) -> Option<EmptyAction> {
    let mut action = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(18.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Open object audio").size(20.0).strong());
            ui.label("Drop an E-AC-3 JOC, TrueHD Atmos or DAMF master, or choose a file.");
            ui.weak("Supported inputs: .ec3  .eac3  .mlp  .thd  .atmos");
            ui.add_space(8.0);
            if ui.button("Choose File…").clicked() {
                action = Some(EmptyAction::OpenDialog);
            }
            ui.weak("Cmd+O also opens a file.");
        });
        ui.add_space(18.0);

        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.strong("Recent files");
                if settings.recent.is_empty() {
                    ui.weak("No recent files yet.");
                }
                for entry in &settings.recent {
                    let name = entry
                        .path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or("Unknown file");
                    if ui
                        .button(name)
                        .on_hover_text(entry.path.display().to_string())
                        .clicked()
                    {
                        action = Some(EmptyAction::Open(entry.path.clone()));
                    }
                    ui.weak(format!("{} · {} objects", entry.codec, entry.objects));
                }
            });

        ui.add_space(10.0);
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.strong("Plugins");
                    if plugins_ready {
                        ui.label(
                            RichText::new("ready").color(Color32::from_rgb(120, 210, 150)),
                        );
                    } else {
                        ui.label(
                            RichText::new("missing").color(Color32::from_rgb(240, 130, 120)),
                        );
                    }
                });
                if !plugins_ready {
                    ui.weak(
                        "Set GST_PLUGIN_PATH to the directory containing the Salsa plugins, then restart.",
                    );
                }
                if ui.button("Copy diagnostics").clicked() {
                    action = Some(EmptyAction::CopyDiagnostics);
                }
            });
    });
    action
}

pub fn draw_loading(ui: &mut egui::Ui, path: Option<&Path>) -> bool {
    let mut cancel = false;
    ui.vertical_centered(|ui| {
        ui.add_space(60.0);
        ui.spinner();
        ui.add_space(12.0);
        ui.heading("Loading");
        if let Some(path) = path {
            ui.label(path.display().to_string());
        }
        ui.weak("Prerolling the decoder and checking for object audio.");
        ui.add_space(12.0);
        if ui.button("Cancel").clicked() {
            cancel = true;
        }
    });
    cancel
}
