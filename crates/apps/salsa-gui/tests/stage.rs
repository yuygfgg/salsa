use egui::{Color32, Pos2, Rect, Shape, Vec2, epaint::TextShape};
use objectaudio_ir::STANDARD_LAYOUTS;
use salsa_gui::{
    pipeline::OutputMode,
    ui::{stage, theme},
    visual::{ObjectVisual, VisualSnapshot, VisualState},
};

fn snapshot() -> VisualSnapshot {
    let mut positions = vec![[0.0, 0.0, 0.0]];
    positions.extend(
        STANDARD_LAYOUTS
            .iter()
            .find(|layout| layout.name == "9.1.6")
            .unwrap()
            .channels
            .iter()
            .filter(|channel| !channel.is_lfe())
            .map(|channel| channel.adm_position()),
    );
    VisualSnapshot {
        time: gstreamer::ClockTime::ZERO,
        lfe_level: 0.0,
        state: VisualState::Live,
        objects: positions
            .into_iter()
            .enumerate()
            .map(|(id, position)| ObjectVisual {
                id: id as u32 + 1,
                position,
                gain: 1.0,
                enabled: true,
                level: 1.0,
                trail: vec![],
            })
            .collect(),
    }
}

fn render(mode: OutputMode, layout: &str, state: &mut stage::StageState, size: Vec2) -> Vec<Shape> {
    render_snapshot(mode, layout, state, size, &snapshot())
}

fn render_snapshot(
    mode: OutputMode,
    layout: &str,
    state: &mut stage::StageState,
    size: Vec2,
    snapshot: &VisualSnapshot,
) -> Vec<Shape> {
    render_snapshot_with_labels(mode, layout, state, size, snapshot, true)
}

fn render_snapshot_with_labels(
    mode: OutputMode,
    layout: &str,
    state: &mut stage::StageState,
    size: Vec2,
    snapshot: &VisualSnapshot,
    labels_enabled: bool,
) -> Vec<Shape> {
    let ctx = egui::Context::default();
    theme::configure(&ctx);
    let mut shapes = Vec::new();
    // Run the settled layout after egui's initial font/layout pass.
    for _ in 0..2 {
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    stage::draw(
                        ui,
                        Some(snapshot),
                        layout,
                        &mode,
                        state,
                        true,
                        labels_enabled,
                        false,
                        false,
                    );
                });
            },
        );
        shapes = output
            .shapes
            .into_iter()
            .flat_map(|shape| flatten(shape.shape))
            .collect();
    }
    let start = shapes.iter().position(|shape| matches!(shape, Shape::Rect(rect) if rect.fill == Color32::from_rgb(15, 17, 21))).expect("Stage background");
    shapes.split_off(start)
}

fn flatten(shape: Shape) -> Vec<Shape> {
    match shape {
        Shape::Vec(shapes) => shapes.into_iter().flat_map(flatten).collect(),
        shape => vec![shape],
    }
}

fn texts(shapes: &[Shape]) -> Vec<&TextShape> {
    shapes
        .iter()
        .filter_map(|shape| match shape {
            Shape::Text(text) => Some(text),
            _ => None,
        })
        .collect()
}

fn listener_bounds(shapes: &[Shape], alpha: Option<u8>) -> Rect {
    shapes
        .iter()
        .filter_map(|shape| match shape {
            Shape::Mesh(mesh)
                if alpha.is_none_or(|alpha| {
                    mesh.vertices.iter().all(|vertex| vertex.color.a() == alpha)
                }) =>
            {
                Some(mesh.calc_bounds())
            }
            _ => None,
        })
        .fold(Rect::NOTHING, |bounds, rect| bounds.union(rect))
}

fn object_nearest_listener(shapes: &[Shape]) -> &egui::epaint::CircleShape {
    let center = listener_bounds(shapes, None).center();
    shapes
        .iter()
        .filter_map(|shape| match shape {
            Shape::Circle(circle) if circle.fill != Color32::TRANSPARENT => Some(circle),
            _ => None,
        })
        .min_by(|a, b| {
            a.center
                .distance_sq(center)
                .total_cmp(&b.center.distance_sq(center))
        })
        .unwrap()
}

#[test]
fn speaker_labels_match_the_output_layout() {
    for layout in objectaudio_render::layouts() {
        let shapes = render(
            OutputMode::Speaker,
            layout.name,
            &mut stage::StageState::default(),
            Vec2::new(700.0, 500.0),
        );
        let texts = texts(&shapes);
        let labels: Vec<_> = texts
            .iter()
            .map(|text| text.galley.job.text.as_str())
            .collect();
        assert_eq!(
            labels.iter().filter(|&&text| text == "Listener").count(),
            1,
            "{}",
            layout.name
        );
        assert_eq!(
            labels.iter().filter(|&&text| text == "L").count(),
            1,
            "Only the left speaker uses L: {}",
            layout.name
        );
        let lfe: Vec<_> = labels
            .iter()
            .copied()
            .filter(|label| label.starts_with("LFE"))
            .collect();
        let expected = if layout.channels.iter().any(|channel| channel.is_lfe()) {
            "LFE"
        } else {
            "LFE (off)"
        };
        assert_eq!(lfe, vec![expected], "{}", layout.name);
        for channel in layout.channels.iter().filter(|channel| !channel.is_lfe()) {
            assert!(
                labels.contains(&channel.short_name()),
                "Missing {} in {}",
                channel.short_name(),
                layout.name
            );
        }
    }
}

#[test]
fn binaural_listener_stays_visible_over_a_coincident_object() {
    let shapes = render(
        OutputMode::Binaural,
        "9.1.6",
        &mut stage::StageState::default(),
        Vec2::new(700.0, 500.0),
    );
    let texts = texts(&shapes);
    let labels: Vec<_> = texts
        .iter()
        .map(|text| text.galley.job.text.as_str())
        .collect();
    assert!(labels.contains(&"Listener"));
    assert!(!labels.contains(&"L"));
    assert!(labels.contains(&"LFE"));
    let bounds = listener_bounds(&shapes, None);
    assert!(bounds.is_positive(), "Draw a projected listener mesh");
    assert!(bounds.contains(object_nearest_listener(&shapes).center));
    for shape in &shapes {
        if let Shape::Mesh(mesh) = shape {
            assert!(
                mesh.vertices.iter().all(|vertex| vertex.color.a() < 255),
                "Keep coincident audio objects visible through the listener"
            );
        }
    }
}

#[test]
fn headphones_turn_and_tilt_in_world_space() {
    let bounds = |yaw, pitch| {
        let mut state = stage::StageState {
            yaw,
            pitch,
            ..Default::default()
        };
        let shapes = render(
            OutputMode::Binaural,
            "7.1.4",
            &mut state,
            Vec2::new(700.0, 500.0),
        );
        // The headphone shell has a separate material from the head.
        listener_bounds(&shapes, Some(235))
    };
    let rear = bounds(0.0, 0.0);
    let side = bounds(std::f32::consts::FRAC_PI_2, 0.0);
    let top = bounds(0.0, std::f32::consts::FRAC_PI_2);
    assert!(
        rear.width() > side.width() * 2.0,
        "The cups must line up in a side view, not face the camera"
    );
    assert!(
        rear.height() > top.height() * 2.0,
        "The headband must foreshorten in a top view"
    );
}

#[test]
fn listener_size_follows_perspective_zoom() {
    for mode in [OutputMode::Speaker, OutputMode::Binaural] {
        let bounds = |distance| {
            let mut state = stage::StageState {
                distance,
                ..Default::default()
            };
            listener_bounds(
                &render(mode.clone(), "7.1.4", &mut state, Vec2::new(700.0, 500.0)),
                None,
            )
        };
        let ratio = bounds(2.4).width() / bounds(4.8).width();
        assert!(
            (1.8..2.2).contains(&ratio),
            "Listener dimensions must stay fixed in world space: {ratio}"
        );
    }
}

#[test]
fn listener_and_audio_objects_share_depth_order() {
    let snapshot = VisualSnapshot {
        time: gstreamer::ClockTime::ZERO,
        lfe_level: 0.0,
        state: VisualState::Live,
        objects: vec![
            ObjectVisual {
                id: 1,
                position: [0.0, -0.5, 0.0],
                gain: 1.0,
                enabled: true,
                level: 1.0,
                trail: vec![],
            },
            ObjectVisual {
                id: 2,
                position: [0.0, 0.5, 0.0],
                gain: 1.0,
                enabled: true,
                level: 0.0,
                trail: vec![],
            },
        ],
    };
    for yaw in [0.0, std::f32::consts::PI] {
        let mut state = stage::StageState {
            yaw,
            pitch: 0.0,
            ..Default::default()
        };
        let shapes = render_snapshot(
            OutputMode::Binaural,
            "7.1.4",
            &mut state,
            Vec2::new(700.0, 500.0),
            &snapshot,
        );
        let object_index = |radius| {
            shapes.iter().position(|shape| matches!(shape, Shape::Circle(circle) if circle.radius == radius && circle.fill != Color32::TRANSPARENT)).unwrap()
        };
        let (behind, ahead) = if yaw == 0.0 {
            (object_index(3.0), object_index(10.0))
        } else {
            (object_index(10.0), object_index(3.0))
        };
        let mesh_indices: Vec<_> = shapes
            .iter()
            .enumerate()
            .filter_map(|(index, shape)| matches!(shape, Shape::Mesh(_)).then_some(index))
            .collect();
        assert!(!mesh_indices.is_empty());
        assert!(
            mesh_indices
                .iter()
                .all(|&index| behind < index && index < ahead),
            "Objects must change sides of the listener when the camera turns around"
        );
    }
}

#[test]
fn reference_labels_are_readable_and_do_not_overlap() {
    let layout = STANDARD_LAYOUTS
        .iter()
        .find(|layout| layout.name == "9.1.6")
        .unwrap();
    for size in [
        Vec2::new(700.0, 500.0),
        Vec2::new(360.0, 600.0),
        Vec2::new(360.0, 300.0),
    ] {
        for view in [
            stage::StageState::perspective_view,
            stage::StageState::top_view,
            stage::StageState::front_view,
        ] {
            let mut state = stage::StageState::default();
            view(&mut state);
            for distance in [state.distance, 8.0] {
                state.distance = distance;
                let shapes = render(OutputMode::Speaker, layout.name, &mut state, size);
                let texts = texts(&shapes);
                assert!(texts.iter().any(|text| text.galley.job.text == "Listener"));
                for channel in layout.channels.iter().filter(|channel| !channel.is_lfe()) {
                    assert!(
                        texts
                            .iter()
                            .any(|text| text.galley.job.text == channel.short_name()),
                        "Missing {} at {size:?}, yaw {}, pitch {}, distance {}",
                        channel.short_name(),
                        state.yaw,
                        state.pitch,
                        state.distance
                    );
                }
                for (index, text) in texts.iter().enumerate() {
                    let bounds = Rect::from_min_size(text.pos, text.galley.size());
                    assert!(
                        Rect::from_min_size(Pos2::ZERO, size).contains_rect(bounds),
                        "Clipped label: {}",
                        text.galley.job.text
                    );
                    for other in texts.iter().skip(index + 1) {
                        assert!(
                            !bounds.intersects(Rect::from_min_size(other.pos, other.galley.size())),
                            "Overlapping labels: {} and {}",
                            text.galley.job.text,
                            other.galley.job.text
                        );
                    }
                    if text.galley.job.text == "Listener"
                        || layout
                            .channels
                            .iter()
                            .any(|channel| channel.short_name() == text.galley.job.text)
                    {
                        assert!(
                            text.galley.job.sections.iter().all(|section| section
                                .format
                                .font_id
                                .size
                                >= 12.0)
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn lfe_fill_reflects_its_channel_level_and_routing() {
    for (mode, layout, routed) in [
        (OutputMode::Binaural, "7.1.4", true),
        (OutputMode::Binaural, "2.0", true),
        (OutputMode::Speaker, "7.1.4", true),
        (OutputMode::Speaker, "2.0", false),
    ] {
        for visual_state in [
            VisualState::Live,
            VisualState::Stale,
            VisualState::Seeking,
            VisualState::Eos,
        ] {
            for level in [0.0, 0.25, 1.0] {
                let mut snapshot = snapshot();
                snapshot.lfe_level = level;
                snapshot.state = visual_state;
                let shapes = render_snapshot(
                    mode.clone(),
                    layout,
                    &mut stage::StageState::default(),
                    Vec2::new(700.0, 500.0),
                    &snapshot,
                );
                let marker = shapes
                    .iter()
                    .find_map(|shape| match shape {
                        Shape::Rect(rect)
                            if rect.rect.size() == Vec2::new(20.0, 12.0)
                                && rect.stroke.width == 2.0 =>
                        {
                            Some(rect.rect)
                        }
                        _ => None,
                    })
                    .expect("LFE outline");
                let fills: Vec<_> = shapes
                    .iter()
                    .filter_map(|shape| match shape {
                        Shape::Rect(rect)
                            if marker.contains_rect(rect.rect)
                                && rect.fill != Color32::TRANSPARENT =>
                        {
                            Some(rect)
                        }
                        _ => None,
                    })
                    .collect();
                if routed && level > 0.0 {
                    assert_eq!(fills.len(), 1);
                    let fill = fills[0];
                    assert!((fill.rect.width() / marker.shrink(3.0).width() - level).abs() < 1e-5);
                    assert_eq!(
                        fill.fill.a(),
                        if visual_state == VisualState::Live {
                            255
                        } else {
                            90
                        }
                    );
                } else {
                    assert!(
                        fills.is_empty(),
                        "Silent or unrouted LFE must have no level fill"
                    );
                }
            }
        }
    }
}

#[test]
fn lfe_marker_tracks_the_position_directly_behind_the_listener() {
    let mut snapshot = snapshot();
    snapshot.objects.push(ObjectVisual {
        id: 99,
        position: [0.0, -1.0, 0.0],
        gain: 1.0,
        enabled: true,
        level: 0.5,
        trail: vec![],
    });
    for (mode, layout, expected_route) in [
        (OutputMode::Speaker, "7.1.4", "LFE"),
        (OutputMode::Speaker, "2.0", "LFE (off)"),
        (OutputMode::Binaural, "7.1.4", "LFE"),
        (OutputMode::Binaural, "2.0", "LFE"),
    ] {
        let mut previous_position = None;
        let mut top = stage::StageState::default();
        top.top_view();
        for mut state in [
            top,
            stage::StageState {
                yaw: 1.1,
                pitch: 0.9,
                distance: 4.8,
                follow: Some(2),
                ..Default::default()
            },
        ] {
            let shapes = render_snapshot(
                mode.clone(),
                layout,
                &mut state,
                Vec2::new(700.0, 500.0),
                &snapshot,
            );
            let labels: Vec<_> = texts(&shapes)
                .into_iter()
                .filter(|text| text.galley.job.text.starts_with("LFE"))
                .collect();
            assert_eq!(labels.len(), 1);
            let label = labels[0];
            assert_eq!(label.galley.job.text, expected_route);
            let marker = shapes
                .iter()
                .find_map(|shape| match shape {
                    Shape::Rect(rect)
                        if rect.rect.size() == Vec2::new(20.0, 12.0)
                            && rect.stroke.width == 2.0 =>
                    {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .expect("Simple LFE marker");
            let rear_object = shapes
                .iter()
                .find_map(|shape| match shape {
                    Shape::Circle(circle)
                        if circle.radius == 6.5 && circle.fill != Color32::TRANSPARENT =>
                    {
                        Some(circle.center)
                    }
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                marker.center(),
                rear_object,
                "LFE must share the rear object's camera transform"
            );
            if let Some(position) = previous_position {
                assert_ne!(marker.center(), position, "LFE must move with the scene");
            } else {
                let listener = object_nearest_listener(&shapes).center;
                assert!((marker.center().x - listener.x).abs() < 0.01);
                assert!(
                    marker.center().y > listener.y + 20.0,
                    "LFE must sit directly behind the listener in the top view"
                );
            }
            previous_position = Some(marker.center());
        }
    }
}

#[test]
fn object_at_listener_position_can_still_be_selected() {
    let ctx = egui::Context::default();
    theme::configure(&ctx);
    let snapshot = snapshot();
    let mut state = stage::StageState::default();
    let mut draw_frame = |events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 500.0))),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    stage::draw(
                        ui,
                        Some(&snapshot),
                        "7.1.4",
                        &OutputMode::Binaural,
                        &mut state,
                        true,
                        true,
                        false,
                        false,
                    );
                });
            },
        )
    };
    let _ = draw_frame(vec![]);
    let shapes: Vec<_> = draw_frame(vec![])
        .shapes
        .into_iter()
        .flat_map(|shape| flatten(shape.shape))
        .collect();
    let position = object_nearest_listener(&shapes).center;
    let _ = draw_frame(vec![
        egui::Event::PointerMoved(position),
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        },
    ]);
    let output = draw_frame(vec![egui::Event::PointerButton {
        pos: position,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Default::default(),
    }]);
    assert_eq!(state.selected, Some(1));
    let shapes: Vec<_> = output
        .shapes
        .into_iter()
        .flat_map(|shape| flatten(shape.shape))
        .collect();
    assert!(
        texts(&shapes)
            .last()
            .unwrap()
            .galley
            .job
            .text
            .starts_with("ID 1 "),
        "Draw the tooltip above reference labels"
    );
}

fn many_objects() -> VisualSnapshot {
    let mut snapshot = snapshot();
    snapshot.objects = (0..17)
        .map(|index| {
            let column = (index % 6) as f32;
            let row = (index / 6) as f32;
            ObjectVisual {
                id: index as u32 + 1,
                position: [
                    -1.0 + column * 0.4,
                    1.0 - row * 0.8,
                    (index % 3) as f32 * 0.4,
                ],
                gain: 1.0,
                enabled: true,
                level: 0.8,
                trail: vec![],
            }
        })
        .collect();
    snapshot
}

#[test]
fn labels_toggle_controls_every_stage_label() {
    let snapshot = many_objects();
    let mut state = stage::StageState::default();
    let size = Vec2::new(1100.0, 800.0);
    let with_labels = render_snapshot_with_labels(
        OutputMode::Speaker,
        "9.1.6",
        &mut state,
        size,
        &snapshot,
        true,
    );
    let labels: Vec<_> = texts(&with_labels)
        .iter()
        .map(|text| text.galley.job.text.as_str())
        .collect();
    assert!(labels.contains(&"Listener"));
    assert!(labels.iter().any(|label| label.starts_with("LFE")));
    assert!(labels.iter().any(|label| label.starts_with("ID")));

    let mut state = stage::StageState::default();
    let without_labels = render_snapshot_with_labels(
        OutputMode::Speaker,
        "9.1.6",
        &mut state,
        size,
        &snapshot,
        false,
    );
    let labels: Vec<_> = texts(&without_labels)
        .iter()
        .map(|text| text.galley.job.text.as_str())
        .collect();
    assert!(!labels.contains(&"Listener"));
    assert!(!labels.iter().any(|label| label.starts_with("LFE")));
    assert!(!labels.iter().any(|label| label.starts_with("ID")));
    for channel in STANDARD_LAYOUTS
        .iter()
        .find(|layout| layout.name == "9.1.6")
        .unwrap()
        .channels
        .iter()
        .filter(|channel| !channel.is_lfe())
    {
        assert!(
            !labels.contains(&channel.short_name()),
            "speaker label {} should be hidden",
            channel.short_name()
        );
    }
}

#[test]
fn labels_are_not_limited_to_sixteen_objects() {
    let snapshot = many_objects();
    let shapes = render_snapshot_with_labels(
        OutputMode::Speaker,
        "9.1.6",
        &mut stage::StageState::default(),
        Vec2::new(1100.0, 800.0),
        &snapshot,
        true,
    );
    let ids: Vec<_> = texts(&shapes)
        .iter()
        .map(|text| text.galley.job.text.as_str())
        .filter(|label| label.starts_with("ID"))
        .collect();
    assert!(
        ids.iter()
            .any(|id| id[2..].parse::<u32>().is_ok_and(|id| id > 16)),
        "expected an object label above ID16, got {ids:?}"
    );
}

#[test]
fn hover_detail_still_shows_when_labels_are_hidden() {
    let ctx = egui::Context::default();
    theme::configure(&ctx);
    let snapshot = snapshot();
    let mut state = stage::StageState::default();
    let mut draw_frame = |events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 500.0))),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    stage::draw(
                        ui,
                        Some(&snapshot),
                        "7.1.4",
                        &OutputMode::Binaural,
                        &mut state,
                        true,
                        false,
                        false,
                        false,
                    );
                });
            },
        )
    };
    let _ = draw_frame(vec![]);
    let shapes: Vec<_> = draw_frame(vec![])
        .shapes
        .into_iter()
        .flat_map(|shape| flatten(shape.shape))
        .collect();
    let position = object_nearest_listener(&shapes).center;
    let _ = draw_frame(vec![egui::Event::PointerMoved(position)]);
    let output = draw_frame(vec![egui::Event::PointerMoved(position)]);
    let shapes: Vec<_> = output
        .shapes
        .into_iter()
        .flat_map(|shape| flatten(shape.shape))
        .collect();
    assert!(
        texts(&shapes)
            .iter()
            .any(|text| text.galley.job.text.contains("gain")),
        "hover detail must not depend on the static labels"
    );
}
