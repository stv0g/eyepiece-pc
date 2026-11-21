//! Rolling latency plot (per-interval p50s), drawn with the painter.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Shape, Stroke};
use eyepiece_pc::link::stats::Snapshot;
use std::collections::VecDeque;

type Series = (&'static str, Color32, fn(&Snapshot) -> f64);
const SERIES: &[Series] = &[
    ("encode", Color32::from_rgb(150, 150, 160), |s| s.enc_p50_ms),
    ("capture->sent", Color32::from_rgb(90, 160, 240), |s| s.cap_send_p50_ms),
    ("capture->received", Color32::from_rgb(200, 140, 240), |s| s.recv_p50_ms),
    ("capture->shown", Color32::from_rgb(80, 200, 120), |s| s.lat_p50_ms),
    ("capture->shown p95", Color32::from_rgb(230, 170, 70), |s| s.lat_p95_ms),
];

/// `hist`: (seconds since start, snapshot); the last `span_s` seconds are shown.
pub fn latency_plot(ui: &mut egui::Ui, hist: &VecDeque<(f64, Snapshot)>, span_s: f64) {
    let w = ui.available_width();
    let (resp, p) = ui.allocate_painter(egui::vec2(w, 170.0), egui::Sense::hover());
    let r = resp.rect;
    let vis = ui.visuals();
    p.rect_filled(r, 4.0, vis.extreme_bg_color);
    let grid = vis.widgets.noninteractive.bg_stroke.color;
    let weak = vis.weak_text_color();
    let font = FontId::proportional(11.0);

    let t_end = hist.back().map(|x| x.0).unwrap_or(span_s).max(span_s);
    let t0 = t_end - span_s;
    let pts: Vec<&(f64, Snapshot)> = hist.iter().filter(|x| x.0 >= t0).collect();
    let mut ymax = 5.0f64;
    for (_, s) in &pts {
        for (_, _, g) in SERIES {
            let v = g(s);
            if v.is_finite() {
                ymax = ymax.max(v);
            }
        }
    }
    let step = nice_step(ymax / 4.0);
    ymax = (ymax / step).ceil() * step;
    let plot = r.shrink2(egui::vec2(6.0, 6.0));
    let plot = egui::Rect::from_min_max(Pos2::new(plot.min.x + 34.0, plot.min.y + 14.0), Pos2::new(plot.max.x, plot.max.y - 14.0));
    let x = |t: f64| plot.min.x + ((t - t0) / span_s) as f32 * plot.width();
    let y = |v: f64| plot.max.y - (v / ymax) as f32 * plot.height();

    let mut v = 0.0;
    while v <= ymax + 1e-9 {
        p.line_segment([Pos2::new(plot.min.x, y(v)), Pos2::new(plot.max.x, y(v))], Stroke::new(0.5, grid));
        p.text(Pos2::new(plot.min.x - 4.0, y(v)), Align2::RIGHT_CENTER, format!("{v:.0}"), font.clone(), weak);
        v += step;
    }
    p.text(Pos2::new(plot.min.x - 30.0, plot.min.y - 12.0), Align2::LEFT_TOP, "ms", font.clone(), weak);
    p.text(Pos2::new(plot.max.x, plot.max.y + 2.0), Align2::RIGHT_TOP, "now", font.clone(), weak);
    p.text(Pos2::new(plot.min.x, plot.max.y + 2.0), Align2::LEFT_TOP, format!("-{span_s:.0} s"), font.clone(), weak);

    for (_, c, g) in SERIES {
        // break the line at gaps (NaN: no data that interval)
        let mut seg: Vec<Pos2> = vec![];
        for (t, s) in &pts {
            let v = g(s);
            if v.is_finite() {
                seg.push(Pos2::new(x(*t), y(v.min(ymax))));
            } else if !seg.is_empty() {
                p.add(Shape::line(std::mem::take(&mut seg), Stroke::new(1.5, *c)));
            }
        }
        if seg.len() == 1 {
            p.circle_filled(seg[0], 2.0, *c);
        } else if !seg.is_empty() {
            p.add(Shape::line(seg, Stroke::new(1.5, *c)));
        }
    }
    // legend
    let mut lx = plot.min.x + 4.0;
    for (name, c, _) in SERIES {
        let g = p.layout_no_wrap(name.to_string(), font.clone(), *c);
        let wdt = g.size().x;
        p.galley(Pos2::new(lx, r.min.y + 3.0), g, *c);
        lx += wdt + 14.0;
    }
    if pts.is_empty() {
        p.text(plot.center(), Align2::CENTER_CENTER, "no stats yet", FontId::proportional(13.0), weak);
    }
}

fn nice_step(x: f64) -> f64 {
    let e = 10f64.powf(x.max(1e-9).log10().floor());
    let m = x / e;
    e * if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    }
}
