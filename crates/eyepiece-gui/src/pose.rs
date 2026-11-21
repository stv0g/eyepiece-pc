//! Small 3D view of the goggles' head pose, drawn with the egui painter (no 3D library): a
//! goggles-shaped box with a forward arrow, seen from behind and slightly above the wearer.
//! Pose convention (ep_pose_t): degrees, +yaw right, +pitch up, +roll right ear down.

use eframe::egui::{self, Pos2, Shape, Stroke};

type V3 = [f32; 3];

// body model: x right, y up, z backwards (forward = -z, away from the viewer)
const W: f32 = 0.9;
const H: f32 = 0.36;
const D: f32 = 0.3;
const BOX: [V3; 8] = [
    [-W, -H, -D], [W, -H, -D], [W, H, -D], [-W, H, -D], // 0-3 front (lens side)
    [-W, -H, D], [W, -H, D], [W, H, D], [-W, H, D], // 4-7 back
];
const FACES: [([usize; 4], bool); 6] = [
    ([0, 1, 2, 3], true),
    ([5, 4, 7, 6], false),
    ([4, 0, 3, 7], false),
    ([1, 5, 6, 2], false),
    ([3, 2, 6, 7], false),
    ([4, 5, 1, 0], false),
];

/// R = Ry(-yaw) · Rx(pitch) · Rz(-roll)
fn rotate(p: V3, yaw: f32, pitch: f32, roll: f32) -> V3 {
    let [mut x, mut y, mut z] = p;
    let (s, c) = (-roll).to_radians().sin_cos();
    (x, y) = (x * c - y * s, x * s + y * c);
    let (s, c) = pitch.to_radians().sin_cos();
    (y, z) = (y * c - z * s, y * s + z * c);
    let (s, c) = (-yaw).to_radians().sin_cos();
    (x, z) = (x * c + z * s, -x * s + z * c);
    [x, y, z]
}

/// Draw into `rect`. `pose` = (yaw, pitch, roll) in degrees, `None` = no data (dimmed).
pub fn draw(ui: &egui::Ui, rect: egui::Rect, pose: Option<(f32, f32, f32)>) {
    let p = ui.painter_at(rect);
    let vis = ui.visuals();
    let fg = vis.text_color();
    let muted = vis.weak_text_color();
    let accent = vis.selection.bg_fill;
    let panel = vis.faint_bg_color;
    let line = vis.widgets.noninteractive.bg_stroke.color;
    let alpha = if pose.is_some() { 1.0 } else { 0.45 };
    let (yaw, pitch, roll) = pose.unwrap_or((0.0, 0.0, 0.0));

    let (cw, ch) = (rect.width(), rect.height());
    let f = ch.min(cw * 0.75) * 1.2;
    let dist = 4.6;
    let (cx, cy) = (rect.center().x, rect.center().y + ch * 0.08);
    let (s30, c30) = 30f32.to_radians().sin_cos();
    let proj = |q: V3| -> (Pos2, f32) {
        let [x, y, z] = q;
        let (y, z) = (y * c30 - z * s30, y * s30 + z * c30); // look down onto it
        let k = f / (dist - z);
        (Pos2::new(cx + x * k, cy - y * k), z)
    };

    // reference: horizon ring at chin height and a "straight ahead" tick
    let ring: Vec<Pos2> = (0..=48)
        .map(|i| {
            let a = i as f32 / 48.0 * std::f32::consts::TAU;
            proj([a.sin() * 1.5, -0.75, a.cos() * 1.5]).0
        })
        .collect();
    p.add(Shape::line(ring, Stroke::new(1.0, line)));
    p.line_segment([proj([0.0, -0.75, -1.5]).0, proj([0.0, -0.75, -1.9]).0], Stroke::new(1.0, line));

    let r = |q: V3| rotate(q, yaw, pitch, roll);
    let bx: Vec<(Pos2, f32)> = BOX.iter().map(|&q| proj(r(q))).collect();
    let mut faces: Vec<(f32, &([usize; 4], bool))> =
        FACES.iter().map(|fc| (fc.0.iter().map(|&i| bx[i].1).sum::<f32>() / 4.0, fc)).collect();
    faces.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)); // far first
    for (_, (idx, front)) in faces {
        let pts: Vec<Pos2> = idx.iter().map(|&i| bx[i].0).collect();
        let fill = if *front { accent } else { panel };
        let fill = fill.gamma_multiply(alpha * if *front { 0.9 } else { 0.85 });
        p.add(Shape::convex_polygon(pts, fill, Stroke::new(1.0, fg.gamma_multiply(alpha))));
    }

    // the arrow's shadow on the ground ring: yaw only
    let fw = r([0.0, 0.0, -2.2]);
    let len = (fw[0] * fw[0] + fw[2] * fw[2]).sqrt();
    if len > 0.05 {
        let a = proj([0.0, -0.75, 0.0]).0;
        let b = proj([fw[0] / len * 1.5, -0.75, fw[2] / len * 1.5]).0;
        p.add(Shape::dashed_line(&[a, b], Stroke::new(1.5, muted), 3.0, 3.0));
    }

    // forward arrow (looking direction)
    let stroke = Stroke::new(2.0, accent.gamma_multiply(alpha));
    let a0 = proj(r([0.0, 0.0, -D])).0;
    let a1 = proj(r([0.0, 0.0, -2.2])).0;
    p.line_segment([a0, a1], stroke);
    for t in [[-0.22, 0.0, -1.85], [0.22, 0.0, -1.85]] {
        p.line_segment([a1, proj(r(t)).0], stroke);
    }
}

