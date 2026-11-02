use eframe::egui;
use egui::{Align2, Color32, FontId, Sense, Stroke};
use gstreamer as gst;

use crate::ui::format_clock;

#[derive(Debug, Default)]
pub struct TransportState {
    pub position: gst::ClockTime,
    pub duration: Option<gst::ClockTime>,
    pub volume: f32,
    pub playing: bool,
    pub scanning: bool,
    pub visual_enabled: bool,
    pub focus: bool,
    pub drag_target: Option<gst::ClockTime>,
}

#[derive(Debug, Clone, Copy)]
pub enum TransportAction {
    Toggle,
    Seek(gst::ClockTime),
    Relative(f64),
    SetVolume(f32),
    ToggleVisual,
    ToggleFocus,
}

pub fn draw(ui: &mut egui::Ui, state: &mut TransportState) -> Option<TransportAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        if ui.button("-10s").clicked() {
            action = Some(TransportAction::Relative(-10.0));
        }
        if ui.button("-5s").clicked() {
            action = Some(TransportAction::Relative(-5.0));
        }
        let label = if state.playing { "Pause" } else { "Play" };
        if ui.button(label).clicked() {
            action = Some(TransportAction::Toggle);
        }
        if ui.button("+5s").clicked() {
            action = Some(TransportAction::Relative(5.0));
        }
        if ui.button("+10s").clicked() {
            action = Some(TransportAction::Relative(10.0));
        }
        ui.add_space(8.0);
        let duration = state
            .duration
            .map(format_clock)
            .unwrap_or_else(|| "--:--".to_string());
        ui.monospace(format!("{} / {duration}", format_clock(state.position)));

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut volume = state.volume;
            ui.add(
                egui::Slider::new(&mut volume, 0.0..=1.0)
                    .show_value(false)
                    .text("Vol"),
            );
            if (volume - state.volume).abs() > 1e-4 {
                action = Some(TransportAction::SetVolume(volume));
            }
            if ui.toggle_value(&mut state.focus, "Focus").changed() {
                action = Some(TransportAction::ToggleFocus);
            }
            if ui
                .toggle_value(&mut state.visual_enabled, "Stage")
                .changed()
            {
                action = Some(TransportAction::ToggleVisual);
            }
        });
    });

    draw_timeline(ui, state, &mut action);
    action
}

fn draw_timeline(
    ui: &mut egui::Ui,
    state: &mut TransportState,
    action: &mut Option<TransportAction>,
) {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 34.0), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    let bar = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 8.0, rect.center().y - 2.0),
        egui::pos2(rect.right() - 8.0, rect.center().y + 2.0),
    );
    painter.rect_filled(bar, 2.0, Color32::from_gray(58));

    let Some(duration) = state.duration.filter(|value| *value > gst::ClockTime::ZERO) else {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            if state.scanning {
                "Scanning duration..."
            } else {
                "Duration unavailable"
            },
            FontId::proportional(12.0),
            Color32::from_gray(150),
        );
        return;
    };

    let total_ns = duration.nseconds().max(1);
    let x_for = |time: gst::ClockTime| {
        let fraction = (time.nseconds() as f64 / total_ns as f64).clamp(0.0, 1.0) as f32;
        bar.left() + fraction * bar.width()
    };
    let time_for = |x: f32| {
        let fraction = ((x - bar.left()) / bar.width()).clamp(0.0, 1.0) as f64;
        gst::ClockTime::from_nseconds((fraction * total_ns as f64) as u64)
    };

    let shown = state.drag_target.unwrap_or(state.position);
    let accent = Color32::from_rgb(92, 160, 235);
    let filled = egui::Rect::from_min_max(bar.min, egui::pos2(x_for(shown), bar.max.y));
    painter.rect_filled(filled, 2.0, accent);
    let thumb = egui::pos2(x_for(shown), rect.center().y);
    painter.circle_filled(thumb, 5.0, accent);
    painter.circle_stroke(thumb, 5.0, Stroke::new(1.0_f32, Color32::from_gray(220)));

    for index in 0..=4 {
        let time = gst::ClockTime::from_nseconds(total_ns * index / 4);
        let x = x_for(time);
        let y = rect.bottom() - 6.0;
        painter.line_segment(
            [egui::pos2(x, y - 3.0), egui::pos2(x, y)],
            Stroke::new(1.0_f32, Color32::from_gray(90)),
        );
        painter.text(
            egui::pos2(x, y - 5.0),
            Align2::CENTER_BOTTOM,
            format_clock(time),
            FontId::monospace(9.0),
            Color32::from_gray(140),
        );
    }

    let pointer = response.interact_pointer_pos();
    if response.drag_started()
        && let Some(pointer) = pointer
    {
        state.drag_target = Some(time_for(pointer.x));
    }
    if response.dragged()
        && let Some(pointer) = pointer
    {
        state.drag_target = Some(time_for(pointer.x));
    }
    if response.drag_stopped()
        && let Some(target) = state.drag_target.take()
    {
        *action = Some(TransportAction::Seek(target));
    }
    if response.clicked()
        && let Some(pointer) = pointer
    {
        *action = Some(TransportAction::Seek(time_for(pointer.x)));
    }

    if let Some(target) = state.drag_target {
        let x = x_for(target);
        let text = format_clock(target);
        let galley = painter.layout_no_wrap(text, FontId::monospace(11.0), Color32::from_gray(240));
        let tooltip = egui::Rect::from_min_size(
            egui::pos2(
                (x - galley.size().x / 2.0).clamp(rect.left(), rect.right() - galley.size().x),
                rect.top() - galley.size().y - 3.0,
            ),
            galley.size() + egui::vec2(8.0, 2.0),
        );
        painter.rect_filled(tooltip, 4.0, Color32::from_rgb(34, 38, 46));
        painter.galley(
            tooltip.min + egui::vec2(4.0, 1.0),
            galley,
            Color32::from_gray(240),
        );
    }
}
