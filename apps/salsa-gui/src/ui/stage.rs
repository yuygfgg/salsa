use std::sync::Arc;

use crate::visual::{ObjectVisual, VisualSnapshot, VisualState};
use eframe::egui;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use objectaudio_ir::{STANDARD_LAYOUTS, StandardLayout};

use crate::pipeline::OutputMode;

mod listener;

type DepthShape = (f32, egui::Shape);

const STAGE_BACKGROUND: Color32 = Color32::from_rgb(15, 17, 21);
const SPEAKER_COLOR: Color32 = Color32::from_rgb(220, 225, 235);
const LISTENER_COLOR: Color32 = Color32::from_rgb(145, 215, 240);
const LFE_COLOR: Color32 = Color32::from_rgb(240, 208, 168);
// Display convention: LFE sits directly behind the listener on the rear wall.
const LFE_POSITION: [f32; 3] = [0.0, -1.0, 0.0];

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
    labels_enabled: bool,
    reduce_motion: bool,
    stale_hint: bool,
) -> Option<StageAction> {
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
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
    painter.rect_filled(rect, 8.0, STAGE_BACKGROUND);

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

    // Keep HUD text outside the projected scene and its labels.
    let compact = rect.width() < 420.0;
    let scene_rect = Rect::from_min_max(
        rect.min + Vec2::new(16.0, if compact { 52.0 } else { 32.0 }),
        rect.max - Vec2::new(16.0, 38.0),
    );
    let camera = Camera {
        yaw: state.yaw,
        pitch: state.pitch,
        distance: state.distance,
    };
    let follow_offset = if let Some(id) = state.follow {
        snapshot
            .and_then(|snapshot| snapshot.objects.iter().find(|object| object.id == id))
            .map(|object| scene_rect.center() - project(camera, object.position, scene_rect))
            .unwrap_or(Vec2::ZERO)
    } else {
        Vec2::ZERO
    };
    let project_point = |position: [f32; 3]| {
        let (screen, depth) = project_depth(camera, position, scene_rect);
        (screen + follow_offset, depth)
    };
    let project = |position: [f32; 3]| project_point(position).0;

    let scene_painter = painter.with_clip_rect(scene_rect);
    draw_room(&scene_painter, &project);
    let mut labels = StageLabels::new(scene_rect);
    let mut scene = Vec::new();
    let listener_bounds =
        listener::add_to_scene(&mut scene, &project_point, *mode == OutputMode::Binaural);
    if labels_enabled {
        labels.add(
            listener_bounds.expand(3.0),
            "Listener".to_owned(),
            LabelKind::Listener,
            Vec2::new(0.0, 1.0),
        );
    }
    let mut hovered = None;
    if let Some(snapshot) = snapshot {
        hovered = add_objects(
            ui,
            &mut scene,
            scene_rect,
            &response,
            snapshot,
            state,
            &project_point,
            reduce_motion,
            labels_enabled,
            &mut labels,
        );
    }
    scene.sort_by(|a, b| b.0.total_cmp(&a.0));
    scene_painter.extend(scene.into_iter().map(|(_, shape)| shape));

    // Speaker anchors remain reference outlines over the depth-sorted scene.
    let layout = STANDARD_LAYOUTS
        .iter()
        .find(|layout| layout.name == layout_name);
    if *mode == OutputMode::Speaker
        && let Some(layout) = layout
    {
        draw_speakers(
            &scene_painter,
            &project,
            layout,
            labels_enabled,
            &mut labels,
        );
    }
    draw_lfe(
        ui,
        &scene_painter,
        &project,
        mode,
        layout,
        snapshot,
        labels_enabled,
        &mut labels,
    );
    labels.paint(&scene_painter);

    let (status, color) = if stale_hint {
        ("Re-syncing...", Color32::from_gray(170))
    } else {
        match snapshot.map(|snapshot| snapshot.state) {
            Some(VisualState::Live) => ("Live", Color32::from_rgb(120, 210, 150)),
            Some(VisualState::Stale) => ("Stale metadata", Color32::from_rgb(235, 180, 110)),
            Some(VisualState::Seeking) => ("Seeking", Color32::from_gray(170)),
            Some(VisualState::Eos) => ("End of stream", Color32::from_gray(150)),
            None => ("Waiting for metadata...", Color32::from_gray(150)),
        }
    };
    painter.text(
        rect.right_top() + Vec2::new(-10.0, if compact { 28.0 } else { 8.0 }),
        Align2::RIGHT_TOP,
        status,
        FontId::proportional(12.0),
        color,
    );

    painter.text(
        rect.left_top() + Vec2::new(10.0, 8.0),
        Align2::LEFT_TOP,
        "Object positions (metadata)",
        FontId::proportional(12.0),
        Color32::from_gray(160),
    );

    // Hover detail is independent of the static labels: turning labels off
    // must not make an object unreadable.
    if let Some(hit) = hovered {
        draw_object_tooltip(&painter, rect, hit.screen, hit.object);
    }

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
}

fn draw_speakers(
    painter: &egui::Painter,
    project: &impl Fn([f32; 3]) -> Pos2,
    layout: &StandardLayout,
    labels_enabled: bool,
    labels: &mut StageLabels,
) {
    for channel in layout.channels {
        if channel.is_lfe() {
            continue;
        }
        let position = project(channel.adm_position());
        let marker = Rect::from_center_size(position, Vec2::splat(14.0));
        for (width, color) in [(5.0_f32, STAGE_BACKGROUND), (2.0, SPEAKER_COLOR)] {
            painter.rect_stroke(
                marker,
                2.0,
                Stroke::new(width, color),
                egui::StrokeKind::Middle,
            );
        }
        if labels_enabled {
            labels.add(
                marker.expand(3.0),
                channel.short_name().to_owned(),
                LabelKind::Speaker,
                (position - project([0.0, 0.0, 0.0])).normalized(),
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_lfe(
    ui: &egui::Ui,
    painter: &egui::Painter,
    project: &impl Fn([f32; 3]) -> Pos2,
    mode: &OutputMode,
    layout: Option<&StandardLayout>,
    snapshot: Option<&VisualSnapshot>,
    labels_enabled: bool,
    labels: &mut StageLabels,
) {
    // The rear marker is a display convention, not an audio panning position.
    let (text, detail, routed) = match mode {
        OutputMode::Binaural => ("LFE", "Input LFE is rendered to headphones.", true),
        OutputMode::Speaker
            if layout
                .is_some_and(|layout| layout.channels.iter().any(|channel| channel.is_lfe())) =>
        {
            (
                "LFE",
                "Input LFE is routed to the LFE output channel.",
                true,
            )
        }
        OutputMode::Speaker => (
            "LFE (off)",
            "This layout has no LFE output channel. The speaker renderer does not render input LFE.",
            false,
        ),
    };
    let position = project(LFE_POSITION);
    let marker = Rect::from_center_size(position, Vec2::new(20.0, 12.0));
    let color = if routed {
        LFE_COLOR
    } else {
        Color32::from_gray(160)
    };
    let level = snapshot.map_or(0.0, |snapshot| snapshot.lfe_level);
    if routed && level > 0.0 {
        let mut fill = marker.shrink(3.0);
        fill.max.x = fill.min.x + fill.width() * level;
        let alpha = if snapshot.is_some_and(|snapshot| snapshot.state == VisualState::Live) {
            255
        } else {
            90
        };
        painter.rect_filled(
            fill,
            1.0,
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha),
        );
    }
    for (width, color) in [(5.0_f32, STAGE_BACKGROUND), (2.0, color)] {
        painter.rect_stroke(
            marker,
            2.0,
            Stroke::new(width, color),
            egui::StrokeKind::Middle,
        );
    }
    if labels_enabled {
        labels.add(
            marker.expand(3.0),
            text.to_owned(),
            LabelKind::Lfe,
            (position - project([0.0; 3])).normalized(),
        );
    }
    // Hover details stay available even when the static labels are off.
    let hover = marker.expand(4.0).intersect(painter.clip_rect());
    if hover.is_positive() {
        ui.interact(hover, ui.id().with("lfe-route"), Sense::hover())
            .on_hover_text(format!(
                "{detail}\nDecoded LFE level: {:.0}%\nThe rear position is a display convention. LFE has no spatial metadata.",
                level * 100.0
            ));
    }
}

struct ObjectHit<'a> {
    screen: Pos2,
    depth: f32,
    object: &'a ObjectVisual,
}

#[allow(clippy::too_many_arguments)]
fn add_objects<'a>(
    ui: &egui::Ui,
    scene: &mut Vec<DepthShape>,
    rect: Rect,
    response: &egui::Response,
    snapshot: &'a VisualSnapshot,
    state: &mut StageState,
    project: &impl Fn([f32; 3]) -> (Pos2, f32),
    reduce_motion: bool,
    labels_enabled: bool,
    labels: &mut StageLabels,
) -> Option<ObjectHit<'a>> {
    let pointer = ui
        .input(|input| input.pointer.hover_pos())
        .filter(|position| rect.contains(*position));
    let dimmed = snapshot.state != VisualState::Live;
    let mut hovered: Option<ObjectHit<'a>> = None;
    for object in &snapshot.objects {
        let id = object.id;
        let (screen, depth) = project(object.position);
        let base = height_color(object.position[2]);
        let object_alpha = if dimmed || !object.enabled { 90 } else { 255 };
        if !reduce_motion && object.trail.len() > 1 {
            // Each segment keeps the height colour of its older endpoint, so a
            // moving object's current height cannot repaint already drawn
            // history.
            let points: Vec<_> = object
                .trail
                .iter()
                .map(|point| (project(*point), height_color(point[2])))
                .collect();
            for (index, pair) in points.windows(2).enumerate() {
                let alpha =
                    ((index + 1) as f32 / points.len() as f32 * 120.0 * f32::from(object_alpha)
                        / 255.0) as u8;
                let color = pair[0].1;
                scene.push((
                    (pair[0].0.1 + pair[1].0.1) * 0.5,
                    egui::Shape::line_segment(
                        [pair[0].0.0, pair[1].0.0],
                        Stroke::new(
                            1.4_f32,
                            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha),
                        ),
                    ),
                ));
            }
        }

        let radius = 3.0 + object.level * 7.0;
        let marker = Rect::from_center_size(screen, Vec2::splat(radius * 2.0 + 4.0));
        let color = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), object_alpha);
        scene.push((depth, egui::Shape::circle_filled(screen, radius, color)));
        let selected = state.selected == Some(id) || state.follow == Some(id);
        scene.push((
            depth,
            egui::Shape::circle_stroke(
                screen,
                radius,
                Stroke::new(
                    if selected { 2.0_f32 } else { 1.0_f32 },
                    if selected {
                        Color32::from_rgb(240, 242, 248)
                    } else {
                        Color32::from_black_alpha(170)
                    },
                ),
            ),
        ));

        if labels_enabled {
            labels.add(
                marker,
                format!("ID{id}"),
                if selected {
                    LabelKind::SelectedObject
                } else {
                    LabelKind::Object
                },
                Vec2::new(1.0, -1.0),
            );
        }

        if let Some(pointer) = pointer
            && pointer.distance(screen) <= (radius + 7.0).max(13.0)
            && hovered.as_ref().is_none_or(|hit| depth <= hit.depth)
        {
            hovered = Some(ObjectHit {
                screen,
                depth,
                object,
            });
        }
    }

    if response.clicked() {
        state.selected = hovered.as_ref().map(|hit| hit.object.id);
    }
    hovered
}

fn draw_object_tooltip(painter: &egui::Painter, rect: Rect, screen: Pos2, object: &ObjectVisual) {
    let gain_db = if object.gain > 1e-6 {
        20.0 * object.gain.log10()
    } else {
        -120.0
    };
    let text = format!(
        "ID {}   x {:+.2}  y {:+.2}  z {:+.2}\n\
             gain {gain_db:+.1} dB   level {:.0}%{}",
        object.id,
        object.position[0],
        object.position[1],
        object.position[2],
        object.level * 100.0,
        if object.enabled { "" } else { "   disabled" },
    );
    let galley = painter.layout(
        text,
        FontId::monospace(11.0),
        Color32::from_rgb(235, 238, 245),
        (rect.width() - 20.0).max(1.0),
    );
    let size = galley.size() + Vec2::new(10.0, 6.0);
    let position = (screen + Vec2::new(24.0, -24.0)).clamp(
        rect.min + Vec2::splat(4.0),
        (rect.max - size - Vec2::splat(4.0)).max(rect.min + Vec2::splat(4.0)),
    );
    let tip = Rect::from_min_size(position, size);
    painter.rect_filled(tip, 4.0, Color32::from_rgb(20, 22, 27));
    painter.galley(tip.min + Vec2::new(5.0, 3.0), galley, Color32::WHITE);
}

/// Reference labels get space before optional object IDs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LabelKind {
    Listener,
    Lfe,
    Speaker,
    SelectedObject,
    Object,
}

struct StageLabel {
    anchor: Rect,
    text: String,
    kind: LabelKind,
    direction: Vec2,
}

struct StageLabels {
    bounds: Rect,
    occupied: Vec<Rect>,
    pending: Vec<StageLabel>,
}

impl StageLabels {
    fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            occupied: Vec::new(),
            pending: Vec::new(),
        }
    }

    fn reserve(&mut self, rect: Rect) {
        if self.bounds.intersects(rect) {
            self.occupied.push(rect);
        }
    }

    fn add(&mut self, anchor: Rect, text: String, kind: LabelKind, direction: Vec2) {
        self.reserve(anchor);
        if self.bounds.contains(anchor.center()) {
            self.pending.push(StageLabel {
                anchor,
                text,
                kind,
                direction,
            });
        }
    }

    fn place(&self, anchor: Pos2, size: Vec2, preferred: Vec2) -> Option<Rect> {
        if size.x > self.bounds.width() || size.y > self.bounds.height() {
            return None;
        }
        let directions = [
            preferred,
            Vec2::new(1.0, -1.0),
            Vec2::new(-1.0, -1.0),
            Vec2::new(1.0, 1.0),
            Vec2::new(-1.0, 1.0),
            Vec2::new(0.0, -1.0),
            Vec2::new(0.0, 1.0),
            Vec2::new(-1.0, 0.0),
            Vec2::new(1.0, 0.0),
        ];
        for ring in 0..12 {
            let clearance = 14.0 + ring as f32 * 12.0;
            for direction in directions {
                let center = anchor
                    + Vec2::new(
                        direction.x * (size.x * 0.5 + clearance),
                        direction.y * (size.y * 0.5 + clearance),
                    );
                let min = (center - size * 0.5).clamp(self.bounds.min, self.bounds.max - size);
                let rect = Rect::from_min_size(min, size);
                if self
                    .occupied
                    .iter()
                    .all(|other| !other.intersects(rect.expand(3.0)))
                {
                    return Some(rect);
                }
            }
        }
        None
    }

    fn paint(mut self, painter: &egui::Painter) {
        self.pending.sort_by_key(|label| label.kind);
        let mut placed: Vec<(Rect, Arc<egui::Galley>)> = Vec::new();
        for label in std::mem::take(&mut self.pending) {
            let color = match label.kind {
                LabelKind::Listener => LISTENER_COLOR,
                LabelKind::Lfe => LFE_COLOR,
                LabelKind::Speaker => SPEAKER_COLOR,
                LabelKind::SelectedObject | LabelKind::Object => Color32::from_gray(205),
            };
            let font = FontId::monospace(match label.kind {
                LabelKind::Listener | LabelKind::Lfe | LabelKind::Speaker => 12.0,
                LabelKind::SelectedObject | LabelKind::Object => 10.0,
            });
            let galley = painter.layout_no_wrap(label.text, font, color);
            let size = galley.size() + Vec2::new(10.0, 6.0);
            if let Some(rect) = self.place(label.anchor.center(), size, label.direction) {
                self.reserve(rect);
                let end = rect.clamp(label.anchor.center());
                let start = label
                    .anchor
                    .intersects_ray_from_center((end - label.anchor.center()).normalized());
                painter.line_segment(
                    [start, end],
                    Stroke::new(1.0_f32, color.gamma_multiply(0.55)),
                );
                placed.push((rect, galley));
            }
        }
        // Paint all label backgrounds last so leader lines cannot cross text.
        for (rect, galley) in placed {
            painter.rect_filled(rect, 3.0, Color32::from_rgb(25, 29, 36));
            painter.galley(rect.min + Vec2::new(5.0, 3.0), galley, Color32::WHITE);
        }
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
    let scale = rect.width().min(rect.height()) * 0.7 / depth;
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
