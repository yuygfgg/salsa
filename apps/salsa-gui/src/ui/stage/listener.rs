//! Listener geometry in ADM coordinates: x right, y forward, z up.
//! The origin is the midpoint between the ears.

use std::{f32::consts::PI, sync::OnceLock};

use egui::{Color32, Mesh, Pos2, Rect, Shape};

use super::DepthShape;

struct Triangle {
    vertices: [[f32; 3]; 3],
    color: Color32,
}

struct ListenerModel {
    head: Vec<Triangle>,
    headphones: Vec<Triangle>,
}

pub(super) fn add_to_scene(
    scene: &mut Vec<DepthShape>,
    project: &impl Fn([f32; 3]) -> (Pos2, f32),
    binaural: bool,
) -> Rect {
    static MODEL: OnceLock<ListenerModel> = OnceLock::new();
    let model = MODEL.get_or_init(ListenerModel::new);
    let headphones = if binaural {
        model.headphones.as_slice()
    } else {
        &[]
    };
    let mut bounds = Rect::NOTHING;
    for triangle in model.head.iter().chain(headphones) {
        let points = triangle.vertices.map(project);
        for (position, _) in points {
            bounds.extend_with(position);
        }
        let ab = points[1].0 - points[0].0;
        let ac = points[2].0 - points[0].0;
        // Front-facing triangles have negative signed screen-space area.
        if ab.x * ac.y - ab.y * ac.x >= 0.0 {
            continue;
        }
        let mut mesh = Mesh::default();
        for (position, _) in points {
            mesh.colored_vertex(position, triangle.color);
        }
        mesh.add_triangle(0, 1, 2);
        let depth = points.iter().map(|point| point.1).sum::<f32>() / 3.0;
        scene.push((depth, Shape::mesh(mesh)));
    }
    bounds
}

impl ListenerModel {
    fn new() -> Self {
        let skin = [145, 215, 240, 185];
        let mut head = Vec::new();
        ellipsoid(&mut head, [0.0; 3], [0.12, 0.145, 0.18], skin);
        ellipsoid(&mut head, [0.0, 0.145, 0.0], [0.032, 0.065, 0.045], skin);
        for side in [-1.0, 1.0] {
            ellipsoid(
                &mut head,
                [side * 0.125, 0.0, 0.0],
                [0.025, 0.038, 0.065],
                skin,
            );
            ellipsoid(
                &mut head,
                [side * 0.045, 0.132, 0.045],
                [0.012, 0.014, 0.014],
                [20, 45, 55, 230],
            );
        }

        let mut headphones = Vec::new();
        let shell = [90, 170, 205, 235];
        for side in [-1.0, 1.0] {
            ellipsoid(
                &mut headphones,
                [side * 0.155, 0.0, 0.0],
                [0.03, 0.055, 0.08],
                shell,
            );
        }
        // A solid band arches over the head in the x/z plane.
        let section = |angle: f32| {
            let (sin, cos) = angle.sin_cos();
            [
                [0.16 * cos, -0.022, 0.195 * sin],
                [0.185 * cos, -0.022, 0.22 * sin],
                [0.185 * cos, 0.022, 0.22 * sin],
                [0.16 * cos, 0.022, 0.195 * sin],
            ]
        };
        for index in 0..16 {
            let a = section(PI * index as f32 / 16.0);
            let b = section(PI * (index + 1) as f32 / 16.0);
            for edge in 0..4 {
                let next = (edge + 1) % 4;
                quad(&mut headphones, [a[edge], a[next], b[next], b[edge]], shell);
            }
        }
        let mut start = section(0.0);
        start.reverse();
        quad(&mut headphones, start, shell);
        quad(&mut headphones, section(PI), shell);
        Self { head, headphones }
    }
}

fn ellipsoid(triangles: &mut Vec<Triangle>, center: [f32; 3], radii: [f32; 3], color: [u8; 4]) {
    const SLICES: usize = 16;
    const STACKS: usize = 8;
    let point = |latitude: usize, longitude: usize| {
        let (sin_lat, cos_lat) = (PI * (latitude as f32 / STACKS as f32 - 0.5)).sin_cos();
        let (sin_lon, cos_lon) = (2.0 * PI * longitude as f32 / SLICES as f32).sin_cos();
        [
            center[0] + radii[0] * cos_lat * cos_lon,
            center[1] + radii[1] * cos_lat * sin_lon,
            center[2] + radii[2] * sin_lat,
        ]
    };
    for latitude in 0..STACKS {
        for longitude in 0..SLICES {
            quad(
                triangles,
                [
                    point(latitude, longitude),
                    point(latitude, longitude + 1),
                    point(latitude + 1, longitude + 1),
                    point(latitude + 1, longitude),
                ],
                color,
            );
        }
    }
}

fn quad(triangles: &mut Vec<Triangle>, points: [[f32; 3]; 4], color: [u8; 4]) {
    for vertices in [
        [points[0], points[1], points[2]],
        [points[0], points[2], points[3]],
    ] {
        let ab: [f32; 3] = std::array::from_fn(|axis| vertices[1][axis] - vertices[0][axis]);
        let ac: [f32; 3] = std::array::from_fn(|axis| vertices[2][axis] - vertices[0][axis]);
        let normal = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        let length = normal.iter().map(|value| value * value).sum::<f32>().sqrt();
        if length < 1e-8 {
            continue;
        }
        // Fixed world-space lighting makes changes of view reveal the volume.
        let light = (normal[0] * -0.36 + normal[1] * -0.48 + normal[2] * 0.8) / length;
        let shade = 0.55 + 0.45 * light.max(0.0);
        triangles.push(Triangle {
            vertices,
            color: Color32::from_rgba_unmultiplied(
                (f32::from(color[0]) * shade) as u8,
                (f32::from(color[1]) * shade) as u8,
                (f32::from(color[2]) * shade) as u8,
                color[3],
            ),
        });
    }
}
