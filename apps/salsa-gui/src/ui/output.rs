use std::path::PathBuf;

use eframe::egui;
use egui::{Color32, RichText};

use crate::pipeline::{
    DeviceInfo, DeviceSelection, OutputMode, layout_channel_count, layout_names,
};
use crate::settings::Settings;

pub enum OutputAction {
    Mode(OutputMode),
    Layout(String),
    Device(DeviceSelection),
    Model(Option<PathBuf>),
    Filterbank(u32),
    Profile(String),
}

pub fn draw_output(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    devices: &[DeviceInfo],
    effective_layout: &str,
    warning: Option<&str>,
) -> Option<OutputAction> {
    let mut action = None;
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.strong("Output");
            ui.label("Virtualization");
            ui.horizontal(|ui| {
                let mut mode = settings.mode.clone();
                ui.selectable_value(&mut mode, OutputMode::Speaker, "Speaker");
                ui.selectable_value(&mut mode, OutputMode::Binaural, "Binaural");
                if mode != settings.mode {
                    settings.mode = mode.clone();
                    action = Some(OutputAction::Mode(mode));
                }
            });

            match settings.mode {
                OutputMode::Speaker => {
                    draw_speaker(ui, settings, devices, effective_layout, &mut action)
                }
                OutputMode::Binaural => draw_binaural(ui, settings, &mut action),
            }

            if settings.mode == OutputMode::Speaker
                && let Some(warning) = warning
            {
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(warning).color(Color32::from_rgb(235, 180, 110)),
                    )
                    .wrap(),
                );
                if ui.button("Switch to Binaural").clicked() {
                    action = Some(OutputAction::Mode(OutputMode::Binaural));
                }
            }
        });
    action
}

fn draw_speaker(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    devices: &[DeviceInfo],
    effective_layout: &str,
    action: &mut Option<OutputAction>,
) {
    ui.horizontal(|ui| {
        ui.label("Layout");
        let mut layout = settings.layout.clone();
        egui::ComboBox::from_id_salt("layout-combo")
            .selected_text(layout.clone())
            .show_ui(ui, |ui| {
                for name in layout_names() {
                    ui.selectable_value(&mut layout, name.to_string(), name);
                }
            });
        if layout != settings.layout {
            settings.layout = layout.clone();
            *action = Some(OutputAction::Layout(layout));
        }
    });
    if effective_layout != settings.layout {
        ui.weak(format!(
            "Rendering {} because of the device limits.",
            effective_layout
        ));
    }

    ui.horizontal(|ui| {
        ui.label("Device");
        let mut device = settings.device.clone();
        let selected = match &device {
            DeviceSelection::Auto => "Auto".to_string(),
            DeviceSelection::Named(name) => name.clone(),
        };
        egui::ComboBox::from_id_salt("device-combo")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut device, DeviceSelection::Auto, "Auto");
                for info in devices {
                    let label = match info.max_channels {
                        Some(channels) => format!("{} ({} ch)", info.display_name, channels),
                        None => info.display_name.clone(),
                    };
                    ui.selectable_value(
                        &mut device,
                        DeviceSelection::Named(info.display_name.clone()),
                        label,
                    );
                }
                if devices.is_empty() {
                    ui.weak("No audio devices reported");
                }
            });
        if device != settings.device {
            settings.device = device.clone();
            *action = Some(OutputAction::Device(device));
        }
    });
    if let Some(channels) = layout_channel_count(&settings.layout) {
        ui.weak(format!("{channels} rendered channels"));
    }
}

fn draw_binaural(ui: &mut egui::Ui, settings: &mut Settings, action: &mut Option<OutputAction>) {
    ui.horizontal(|ui| {
        ui.label("Model");
        match &settings.binaural.model {
            Some(path) => {
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("configured"),
                        )
                        .weak(),
                    )
                    .truncate(),
                );
            }
            None => {
                ui.colored_label(Color32::from_rgb(240, 160, 130), "not configured");
            }
        }
    });
    if ui.button("Choose model…").clicked()
        && let Some(path) = rfd::FileDialog::new()
            .set_title("Choose a Rosella personalized-headphone model")
            .add_filter("Rosella model", &["personalized_headphone"])
            .pick_file()
    {
        *action = Some(OutputAction::Model(Some(path)));
    }
    if settings.binaural.model.is_none() {
        ui.colored_label(
            Color32::from_rgb(240, 160, 130),
            "Choose a model to enable binaural playback.",
        );
    }

    egui::CollapsingHeader::new("Advanced")
        .default_open(false)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Filterbank");
                let mut filterbank = settings.binaural.filterbank;
                egui::ComboBox::from_id_salt("filterbank-combo")
                    .selected_text(match filterbank {
                        1 => "1 - public".to_string(),
                        _ => "2 - captured".to_string(),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut filterbank, 1, "1 - public");
                        ui.selectable_value(&mut filterbank, 2, "2 - captured");
                    });
                if filterbank != settings.binaural.filterbank {
                    settings.binaural.filterbank = filterbank;
                    *action = Some(OutputAction::Filterbank(filterbank));
                }
            });
            ui.horizontal(|ui| {
                ui.label("Profile");
                let mut profile = settings.binaural.profile.clone();
                egui::ComboBox::from_id_salt("profile-combo")
                    .selected_text(profile.clone())
                    .show_ui(ui, |ui| {
                        for name in ["near", "mid", "far"] {
                            ui.selectable_value(&mut profile, name.to_string(), name);
                        }
                    });
                if profile != settings.binaural.profile {
                    settings.binaural.profile = profile.clone();
                    *action = Some(OutputAction::Profile(profile));
                }
            });
            ui.weak("The decoder already folds the codec delay into object metadata.");
        });
}
