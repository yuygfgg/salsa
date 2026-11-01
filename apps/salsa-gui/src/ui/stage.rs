use std::cmp::Ordering;

use crate::visual::{ObjectVisual, VisualSnapshot, VisualState};
use eframe::egui;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use objectaudio_ir::STANDARD_LAYOUTS;

use crate::pipeline::OutputMode;

#[derive(Debug)]
pub struct StageState {
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
    pub selected: Option<u32>,
    pub follow: Option<u32>,
}

impl Default for StageState {
    fn default() -> Self {
        Self {
            yaw: 0.55,
            pitch: 0.35,
            distance: 3.2,
            selected: None,
            follow: None,
        }
    }
}

impl StageState {
    pub fn top_view(&mut self) {
        self.yaw = 0.0;
        self.pitch = std::f32::consts::FRAC_PI_2 - 0.01;
        self.distance = 3.0;
    }

    pub fn front_view(&mut self) {
        self.yaw = 0.0;
        self.pitch = 0.08;
        self.distance = 3.2;
    }

    pub fn perspective_view(&mut self) {
        self.yaw = 0.55;
        self.pitch = 0.35;
        self.distance = 3.2;
    }

    pub fn follow_selected(&mut self) {
        if let Some(selected) = self.selected {
            self.follow = if self.follow == Some(selected) {
                None
            } else {
                Some(selected)
            };
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageAction {
    ToggleFocus,
}

#[allow(clippy::too_many_arguments)]
pub fn draw(
    ui: &mut egui::Ui,
    snapshot: Option<&VisualSnapshot>,
    layout_name: &str,
    mode: &OutputMode,
    state: &mut StageState,
    visual_enabled: bool,
    reduce_motion: bool,
    stale_hint: bool,
) -> Option<StageAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        if ui.button("Top").clicked() {
            state.top_view();
        }
        if ui.button("Front").clicked() {
            state.front_view();
        }
        if ui.button("Reset").clicked() {
            state.perspective_view();
        }
        if ui.button("Follow").clicked() {
            state.follow_selected();
        }
        if ui.button("Focus").clicked() {
            action = Some(StageAction::ToggleFocus);
        }
        ui.weak("Drag to orbit, scroll to zoom, double-click to reset.");
    });

    if !visual_enabled {
        ui.centered_and_justified(|ui| {
            ui.weak("The 3D object stage is off. Press V or enable it in Settings.");
        });
        return action;
    }

    let size = ui.available_size();
    if size.x < 180.0 || size.y < 180.0 {
        ui.centered_and_justified(|ui| {
            ui.weak("Make the window larger to show the 3D stage.");
        });
        return action;
    }
    let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
    let rect = response.rect;
    painter.rect_filled(rect, 8.0, Color32::from_rgb(15, 17, 21));

    if response.dragged_by(egui::PointerButton::Primary) {
        let delta = ui.input(|input| input.pointer.delta());
        state.yaw += delta.x * 0.008;
        state.pitch = (state.pitch + delta.y * 0.008).clamp(-1.45, 1.45);
    }
    if response.hovered() {
        let scroll = ui.input(|input| input.smooth_scroll_delta.y);
        if scroll.abs() > 0.0 {
            state.distance = (state.distance * (1.0 - scroll * 0.0015)).clamp(1.2, 8.0);
        }
    }
    if response.double_clicked() {
        state.perspective_view();
    }

    let camera = Camera {
        yaw: state.yaw,
        pitch: state.pitch,
        distance: state.distance,
    };
    let follow_offset = if let Some(id) = state.follow {
        snapshot
            .and_then(|snapshot| snapshot.objects.iter().find(|object| object.id == id))
            .map(|object| rect.center() - project(camera, object.position, rect))
            .unwrap_or(Vec2::ZERO)
    } else {
        Vec2::ZERO
    };
    let project = |position: [f32; 3]| project(camera, position, rect) + follow_offset;

    draw_room(&painter, &project);

    match mode {
        OutputMode::Speaker => draw_speakers(&painter, &project, layout_name),
        OutputMode::Binaural => draw_head(&painter, project([0.0, 0.0, 0.0])),
    }

    if let Some(snapshot) = snapshot {
        draw_objects(
            ui,
            &painter,
            rect,
            &response,
            snapshot,
            state,
            camera,
            &project,
            reduce_motion,
        );
        let (label, color) = match snapshot.state {
            VisualState::Live => ("Live", Color32::from_rgb(120, 210, 150)),
            VisualState::Stale => ("Stale metadata", Color32::from_rgb(235, 180, 110)),
            VisualState::Seeking => ("Seeking", Color32::from_gray(170)),
            VisualState::Eos => ("End of stream", Color32::from_gray(150)),
        };
        painter.text(
            rect.right_top() + Vec2::new(-10.0, 8.0),
            Align2::RIGHT_TOP,
            label,
            FontId::proportional(12.0),
            color,
        );
    } else {
        painter.text(
            rect.center() + Vec2::new(0.0, 48.0),
            Align2::CENTER_CENTER,
            "Waiting for object metadata...",
            FontId::proportional(15.0),
            Color32::from_gray(150),
        );
    }

    painter.text(
        rect.left_top() + Vec2::new(10.0, 8.0),
        Align2::LEFT_TOP,
        "Object positions (metadata)",
        FontId::proportional(12.0),
        Color32::from_gray(160),
    );

    if stale_hint {
        painter.text(
            rect.center() + Vec2::new(0.0, 26.0),
            Align2::CENTER_CENTER,
            "Re-syncing...",
            FontId::proportional(13.0),
            Color32::from_gray(150),
        );
    }

    let _ = response;
    action
}

fn draw_room(painter: &egui::Painter, project: &impl Fn([f32; 3]) -> Pos2) {
    let floor = Stroke::new(1.0_f32, Color32::from_rgb(38, 43, 52));
    for index in -4..=4 {
        let value = index as f32 / 4.0;
        painter.line_segment(
            [project([-1.0, value, 0.0]), project([1.0, value, 0.0])],
            floor,
        );
        painter.line_segment(
            [project([value, -1.0, 0.0]), project([value, 1.0, 0.0])],
            floor,
        );
    }
    let height = Stroke::new(1.0_f32, Color32::from_rgb(32, 37, 45));
    for z in [0.25, 0.5, 0.75, 1.0] {
        for (a, b) in [
            ([-1.0, -1.0], [1.0, -1.0]),
            ([1.0, -1.0], [1.0, 1.0]),
            ([1.0, 1.0], [-1.0, 1.0]),
            ([-1.0, 1.0], [-1.0, -1.0]),
        ] {
            painter.line_segment([project([a[0], a[1], z]), project([b[0], b[1], z])], height);
        }
    }
    for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        painter.line_segment([project([x, y, 0.0]), project([x, y, 1.0])], height);
    }

    let listener = project([0.0, 0.0, 0.0]);
    painter.circle_filled(listener, 4.0, Color32::from_rgb(190, 195, 205));
    painter.text(
        listener + Vec2::new(8.0, 6.0),
        Align2::LEFT_TOP,
        "L",
        FontId::monospace(10.0),
        Color32::from_gray(170),
    );
}

fn draw_speakers(painter: &egui::Painter, project: &impl Fn([f32; 3]) -> Pos2, layout_name: &str) {
    let Some(layout) = STANDARD_LAYOUTS
        .iter()
        .find(|layout| layout.name == layout_name)
    else {
        return;
    };
    for channel in layout.channels {
        if channel.is_lfe() {
            continue;
        }
        draw_anchor(
            painter,
            project(channel.adm_position()),
            channel.short_name(),
        );
    }
    let lfe = project([0.0, 0.0, 0.0]) + Vec2::new(0.0, 22.0);
    let box_rect = Rect::from_center_size(lfe, Vec2::new(26.0, 11.0));
    painter.rect_filled(box_rect, 2.0, Color32::from_rgb(96, 74, 58));
    painter.rect_stroke(
        box_rect,
        2.0,
        Stroke::new(1.0_f32, Color32::from_rgb(160, 128, 96)),
        egui::StrokeKind::Inside,
    );
    painter.text(
        lfe,
        Align2::CENTER_CENTER,
        "LFE",
        FontId::monospace(8.0),
        Color32::from_rgb(220, 200, 180),
    );
}

fn draw_anchor(painter: &egui::Painter, position: Pos2, name: &str) {
    let rect = Rect::from_center_size(position, Vec2::splat(6.0));
    painter.rect_filled(rect, 1.0, Color32::from_rgb(135, 140, 150));
    painter.text(
        position + Vec2::new(7.0, -5.0),
        Align2::LEFT_BOTTOM,
        name,
        FontId::monospace(9.0),
        Color32::from_rgb(145, 150, 160),
    );
}

fn draw_head(painter: &egui::Painter, position: Pos2) {
    painter.circle_stroke(
        position,
        9.0,
        Stroke::new(1.5_f32, Color32::from_rgb(170, 175, 190)),
    );
    painter.circle_filled(position, 3.0, Color32::from_rgb(170, 175, 190));
    painter.circle_filled(
        position + Vec2::new(-10.0, 0.0),
        2.5,
        Color32::from_rgb(150, 155, 170),
    );
    painter.circle_filled(
        position + Vec2::new(10.0, 0.0),
        2.5,
        Color32::from_rgb(150, 155, 170),
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_objects(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    response: &egui::Response,
    snapshot: &VisualSnapshot,
    state: &mut StageState,
    camera: Camera,
    project: &impl Fn([f32; 3]) -> Pos2,
    reduce_motion: bool,
) {
    let pointer = ui.input(|input| input.pointer.hover_pos());
    let mut screens: Vec<(u32, Pos2, f32, &ObjectVisual)> = snapshot
        .objects
        .iter()
        .map(|object| {
            let depth = project_depth(camera, object.position, rect).1;
            (object.id, project(object.position), depth, object)
        })
        .collect();
    screens.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(Ordering::Equal));

    let dimmed = snapshot.state != VisualState::Live;
    let mut hovered: Option<(u32, Pos2, &ObjectVisual)> = None;
    for (id, screen, _depth, object) in &screens {
        let base = height_color(object.position[2]);
        let object_alpha = if dimmed || !object.enabled { 90 } else { 255 };
        if !reduce_motion && object.trail.len() > 1 {
            let points: Vec<Pos2> = object.trail.iter().map(|point| project(*point)).collect();
            for (index, pair) in points.windows(2).enumerate() {
                let alpha =
                    ((index + 1) as f32 / points.len() as f32 * 120.0 * f32::from(object_alpha)
                        / 255.0) as u8;
                painter.line_segment(
                    [pair[0], pair[1]],
                    Stroke::new(
                        1.4_f32,
                        Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha),
                    ),
                );
            }
        }

        let radius = 3.0 + object.level * 7.0;
        let color = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), object_alpha);
        painter.circle_filled(*screen, radius, color);
        let selected = state.selected == Some(*id) || state.follow == Some(*id);
        painter.circle_stroke(
            *screen,
            radius,
            Stroke::new(
                if selected { 2.0_f32 } else { 1.0_f32 },
                if selected {
                    Color32::from_rgb(240, 242, 248)
                } else {
                    Color32::from_black_alpha(170)
                },
            ),
        );

        if snapshot.objects.len() <= 16 || selected {
            painter.text(
                *screen + Vec2::new(radius + 3.0, -radius),
                Align2::LEFT_BOTTOM,
                format!("ID{id}"),
                FontId::monospace(9.0),
                Color32::from_gray(205),
            );
        }

        if let Some(pointer) = pointer
            && pointer.distance(*screen) <= (radius + 7.0).max(13.0)
        {
            hovered = Some((*id, *screen, object));
        }
    }

    if let Some((id, screen, object)) = hovered {
        let gain_db = if object.gain > 1e-6 {
            20.0 * object.gain.log10()
        } else {
            -120.0
        };
        let text = format!(
            "ID {id}   x {:+.2}  y {:+.2}  z {:+.2}\n\
             gain {gain_db:+.1} dB   level {:.0}%{}",
            object.position[0],
            object.position[1],
            object.position[2],
            object.level * 100.0,
            if object.enabled { "" } else { "   disabled" },
        );
        let galley = painter.layout_no_wrap(
            text,
            FontId::monospace(10.0),
            Color32::from_rgb(235, 238, 245),
        );
        let size = galley.size() + Vec2::new(10.0, 6.0);
        let mut position = screen + Vec2::new(12.0, -12.0);
        if position.x + size.x > rect.right() {
            position.x = rect.right() - size.x - 4.0;
        }
        if position.y < rect.top() {
            position.y = rect.top() + 4.0;
        }
        let tip = Rect::from_min_size(position, size);
        painter.rect_filled(tip, 4.0, Color32::from_rgba_unmultiplied(20, 22, 27, 235));
        painter.galley(
            tip.min + Vec2::new(5.0, 3.0),
            galley,
            Color32::from_rgb(235, 238, 245),
        );
    }

    if response.clicked() {
        state.selected = hovered.map(|(id, _, _)| id);
    }
}

#[derive(Clone, Copy)]
struct Camera {
    yaw: f32,
    pitch: f32,
    distance: f32,
}

fn project(camera: Camera, position: [f32; 3], rect: Rect) -> Pos2 {
    project_depth(camera, position, rect).0
}

fn project_depth(camera: Camera, position: [f32; 3], rect: Rect) -> (Pos2, f32) {
    let (yaw_sin, yaw_cos) = camera.yaw.sin_cos();
    let x1 = position[0] * yaw_cos - position[1] * yaw_sin;
    let y1 = position[0] * yaw_sin + position[1] * yaw_cos;
    let (pitch_sin, pitch_cos) = camera.pitch.sin_cos();
    let y2 = y1 * pitch_cos - position[2] * pitch_sin;
    let z2 = y1 * pitch_sin + position[2] * pitch_cos;
    let depth = (camera.distance + y2).max(0.6);
    let scale = rect.height() * 0.7 / depth;
    let screen = rect.center() + Vec2::new(x1 * scale, -z2 * scale);
    (screen, depth)
}

fn height_color(height: f32) -> Color32 {
    let height = height.clamp(0.0, 1.0);
    let stops = [
        (0.0, [0.25, 0.55, 0.95]),
        (0.33, [0.20, 0.85, 0.75]),
        (0.66, [0.95, 0.85, 0.25]),
        (1.0, [0.95, 0.35, 0.25]),
    ];
    for pair in stops.windows(2) {
        let (start, a) = pair[0];
        let (end, b) = pair[1];
        if height <= end {
            let t = ((height - start) / (end - start)).clamp(0.0, 1.0);
            let mix = |x: f32, y: f32| ((x + (y - x) * t) * 255.0) as u8;
            return Color32::from_rgb(mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2]));
        }
    }
    Color32::from_rgb(242, 89, 64)
}
