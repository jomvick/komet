//! Dotted thought-orb loading indicators for AI & agent interfaces.
//!
//! Ported from Jakub Antalik's `thinking-orbs` (<https://orbs.jakubantalik.com/>).
//! Pure geometry & math with GPU-accelerated rendering in GPUI.
//!
//! Features:
//! - 9 tuned states: `working`, `searching`, `solving`, `listening`, `connecting`,
//!   `weaving`, `composing`, `breathing`, `shaping`.
//! - High-performance GPU-rendered rounded quads for anti-aliased dots & stroke paths for lines.
//! - Accurate 3D projection, depth shading, and z-sorting.
//! - Light / Dark theme awareness (`white` ink inversion).
//! - Automatic frame throttling and `reduce_motion` accessibility support.

use std::time::Instant;

use gpui::{
    AnyElement, BorderStyle, Bounds, IntoElement, ParentElement, PathBuilder, Styled, canvas, div,
    point, px, quad, size,
};

use crate::theme::Theme;

/// The 9 animation states for ThinkingOrb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OrbState {
    #[default]
    Working,
    Searching,
    Solving,
    Listening,
    Connecting,
    Weaving,
    Composing,
    Breathing,
    Shaping,
}

impl OrbState {
    /// Parse an orb state from its string name.
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_lowercase().as_str() {
            "searching" => Self::Searching,
            "solving" => Self::Solving,
            "listening" => Self::Listening,
            "connecting" => Self::Connecting,
            "weaving" => Self::Weaving,
            "composing" => Self::Composing,
            "breathing" => Self::Breathing,
            "shaping" => Self::Shaping,
            _ => Self::Working,
        }
    }

    /// Map a transcript flavour word or agent action to an OrbState.
    pub fn from_flavour_word(word: &str) -> Self {
        let lower = word.to_lowercase();
        if lower.contains("search") || lower.contains("scout") || lower.contains("find") {
            Self::Searching
        } else if lower.contains("solv") || lower.contains("puzzl") || lower.contains("calculat") {
            Self::Solving
        } else if lower.contains("listen") || lower.contains("send") || lower.contains("hear") {
            Self::Listening
        } else if lower.contains("connect") || lower.contains("network") || lower.contains("wire") {
            Self::Connecting
        } else if lower.contains("weav") || lower.contains("braid") || lower.contains("knit") {
            Self::Weaving
        } else if lower.contains("compos") || lower.contains("write") || lower.contains("draft") {
            Self::Composing
        } else if lower.contains("breath") || lower.contains("rest") || lower.contains("idle") {
            Self::Breathing
        } else if lower.contains("shap") || lower.contains("morph") || lower.contains("build") {
            Self::Shaping
        } else {
            Self::Working
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Working => "Working…",
            Self::Searching => "Searching…",
            Self::Solving => "Solving…",
            Self::Listening => "Listening…",
            Self::Connecting => "Connecting…",
            Self::Weaving => "Weaving…",
            Self::Composing => "Composing…",
            Self::Breathing => "Thinking…",
            Self::Shaping => "Shaping…",
        }
    }
}

/// A rendered dot ready for painting.
#[derive(Debug, Clone, Copy)]
pub struct Dot {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub r: f32,
    pub white: f32,
    pub a: f32,
}

/// A rendered line segment (used in `connecting`).
#[derive(Debug, Clone, Copy)]
pub struct Line {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub white: f32,
    pub a: f32,
    pub w: f32,
}

/// One rendered frame of the orb.
pub struct OrbFrame {
    pub dots: Vec<Dot>,
    pub lines: Vec<Line>,
}

// ---------------------------------------------------------------------------
// Math & 3D Projection Helpers
// ---------------------------------------------------------------------------

fn project_point(
    x: f32,
    y: f32,
    z: f32,
    rot_y: f32,
    cam_tilt: f32,
    cx: f32,
    cy: f32,
    scale: f32,
) -> (f32, f32, f32) {
    let sin_t = rot_y.sin();
    let cos_t = rot_y.cos();
    let sin_c = cam_tilt.sin();
    let cos_c = cam_tilt.cos();

    let x1 = x * cos_t + z * sin_t;
    let z1 = -x * sin_t + z * cos_t;
    let y2 = y * cos_c - z1 * sin_c;
    let z2 = y * sin_c + z1 * cos_c;

    (cx + x1 * scale, cy - y2 * scale, z2)
}

fn hash_d(a: f32, b: f32) -> f32 {
    let h = (a * 12.9898 + b * 78.233).sin() * 43758.5453;
    h - h.floor()
}

fn vnoise(x: f32, y: f32) -> f32 {
    let xi = x.floor();
    let yi = y.floor();
    let mut fx = x - xi;
    let mut fy = y - yi;
    fx = fx * fx * (3.0 - 2.0 * fx);
    fy = fy * fy * (3.0 - 2.0 * fy);
    let a = hash_d(xi, yi);
    let b = hash_d(xi + 1.0, yi);
    let c = hash_d(xi, yi + 1.0);
    let d = hash_d(xi + 1.0, yi + 1.0);
    a + (b - a) * fx + (c - a) * fy + (a - b - c + d) * fx * fy
}

fn fib_dir(i: usize, n: usize) -> [f32; 3] {
    let golden = std::f32::consts::PI * (3.0 - 5.0_f32.sqrt());
    let y = 1.0 - (2.0 * (i as f32 + 0.5)) / (n as f32);
    let rad = (1.0 - y * y).max(0.0).sqrt();
    let theta = i as f32 * golden;
    [rad * theta.cos(), y, rad * theta.sin()]
}

fn radius_scale(size: f32, rs_pow: f32) -> f32 {
    (size / 300.0).powf(rs_pow)
}

fn finalize_frame(mut dots: Vec<Dot>, lines: Vec<Line>, r_min: f32) -> OrbFrame {
    dots.retain(|d| d.a >= 0.02);
    for d in &mut dots {
        d.r = d.r.max(r_min);
    }
    dots.sort_by(|a, b| a.z.partial_cmp(&b.z).unwrap_or(std::cmp::Ordering::Equal));
    let lines = lines.into_iter().filter(|l| l.a >= 0.02).collect();
    OrbFrame { dots, lines }
}

// ---------------------------------------------------------------------------
// Mode Generators
// ---------------------------------------------------------------------------

struct ModePreset {
    speed: f32,
    count: f32,
    size: f32,
}

fn resolve_preset(state: OrbState, size_px: f32) -> ModePreset {
    let t = ((size_px - 20.0) / (64.0 - 20.0)).clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32| a + (b - a) * t;

    match state {
        OrbState::Working => ModePreset {
            speed: lerp(3.9, 1.885),
            count: lerp(0.238, 1.0),
            size: lerp(2.4, 1.0),
        },
        OrbState::Searching => ModePreset {
            speed: lerp(2.0, 1.0),
            count: lerp(0.25, 1.0),
            size: lerp(2.2, 1.0),
        },
        OrbState::Solving => ModePreset {
            speed: lerp(1.8, 1.0),
            count: lerp(0.25, 1.0),
            size: lerp(2.2, 1.0),
        },
        OrbState::Listening => ModePreset {
            speed: lerp(2.0, 1.0),
            count: lerp(0.25, 1.0),
            size: lerp(2.2, 1.0),
        },
        OrbState::Connecting => ModePreset {
            speed: lerp(1.8, 1.0),
            count: lerp(0.4, 1.0),
            size: lerp(2.0, 1.0),
        },
        OrbState::Weaving => ModePreset {
            speed: lerp(2.0, 1.0),
            count: lerp(0.3, 1.0),
            size: lerp(2.2, 1.0),
        },
        OrbState::Composing => ModePreset {
            speed: lerp(2.0, 1.0),
            count: lerp(0.3, 1.0),
            size: lerp(2.2, 1.0),
        },
        OrbState::Breathing => ModePreset {
            speed: lerp(1.5, 1.0),
            count: lerp(0.3, 1.0),
            size: lerp(2.2, 1.0),
        },
        OrbState::Shaping => ModePreset {
            speed: lerp(1.5, 1.0),
            count: lerp(0.4, 1.0),
            size: lerp(2.0, 1.0),
        },
    }
}

pub fn generate_frame(state: OrbState, size: f32, time_secs: f32, speed_override: f32) -> OrbFrame {
    let preset = resolve_preset(state, size);
    let speed = preset.speed * speed_override;
    let count = preset.count;
    let size_mul = preset.size;

    match state {
        OrbState::Working => frame_orbits(size, time_secs, speed, count, size_mul),
        OrbState::Searching => frame_globe(size, time_secs, speed, count, size_mul),
        OrbState::Solving => frame_rubik(size, time_secs, speed, count, size_mul),
        OrbState::Listening => frame_wave(size, time_secs, speed, count, size_mul),
        OrbState::Connecting => frame_web(size, time_secs, speed, count, size_mul),
        OrbState::Weaving => frame_braid(size, time_secs, speed, count, size_mul),
        OrbState::Composing => frame_ribbon(size, time_secs, speed, count, size_mul),
        OrbState::Breathing => frame_ring(size, time_secs, speed, count, size_mul),
        OrbState::Shaping => frame_morph(size, time_secs, speed, count, size_mul),
    }
}

// 1. Orbits (working)
fn frame_orbits(size: f32, t: f32, speed_mul: f32, count_mul: f32, size_mul: f32) -> OrbFrame {
    let cx = size / 2.0;
    let cy = size / 2.0;
    let r_sphere = (size / 2.0) * 0.82;
    let rs = radius_scale(size, 0.6) * size_mul;
    let time = t * speed_mul;

    let mut dots = Vec::new();
    let orbit_n = ((12.0 * count_mul).round() as usize).max(2);
    let ghost_n = ((40.0 * count_mul).round() as usize).max(6);
    let particles = 3;

    for orb in 0..orbit_n {
        let h1 = hash_d(orb as f32, 1.7);
        let h2 = hash_d(orb as f32, 5.2);
        let h3 = hash_d(orb as f32, 8.9);
        let ro = r_sphere * (0.45 + 0.52 * h1);
        let th = h1 * 2.0 * std::f32::consts::PI;
        let phi = (2.0 * h2 - 1.0).clamp(-1.0, 1.0).acos();

        let nx = phi.sin() * th.cos();
        let ny = phi.cos();
        let nz = phi.sin() * th.sin();

        let (ax, ay, az) = if ny.abs() < 0.9 {
            (0.0, 1.0, 0.0)
        } else {
            (1.0, 0.0, 0.0)
        };
        let ux = ny * az - nz * ay;
        let uy = nz * ax - nx * az;
        let uz = nx * ay - ny * ax;
        let u_len = (ux * ux + uy * uy + uz * uz).sqrt().max(1e-6);
        let (ux, uy, uz) = (ux / u_len, uy / u_len, uz / u_len);
        let vx = ny * uz - nz * uy;
        let vy = nz * ux - nx * uz;
        let vz = nx * uy - ny * ux;

        let spd = (0.3 + 0.7 * h3) * (if orb % 2 == 0 { 1.0 } else { -1.0 });

        for g in 0..ghost_n {
            let a = (g as f32 / ghost_n as f32) * 2.0 * std::f32::consts::PI;
            let px = (ux * a.cos() + vx * a.sin()) * ro;
            let py = (uy * a.cos() + vy * a.sin()) * ro;
            let pz = (uz * a.cos() + vz * a.sin()) * ro;
            let (sx, sy, sz) = project_point(px, py, pz, time * 0.12, 0.3, cx, cy, 1.0);
            let depth = (sz / r_sphere + 1.0) / 2.0;
            dots.push(Dot {
                x: sx,
                y: sy,
                z: sz,
                r: 0.65 * rs,
                white: 0.75,
                a: 0.15 + 0.15 * depth,
            });
        }

        for p in 0..particles {
            let ph = (time * spd * 0.18 + p as f32 / particles as f32).rem_euclid(1.0);
            let a = ph * 2.0 * std::f32::consts::PI;
            let px = (ux * a.cos() + vx * a.sin()) * ro;
            let py = (uy * a.cos() + vy * a.sin()) * ro;
            let pz = (uz * a.cos() + vz * a.sin()) * ro;
            let (sx, sy, sz) = project_point(px, py, pz, time * 0.12, 0.3, cx, cy, 1.0);
            let depth = (sz / r_sphere + 1.0) / 2.0;
            dots.push(Dot {
                x: sx,
                y: sy,
                z: sz,
                r: (1.4 + 1.8 * depth) * rs,
                white: 0.6 - 0.45 * depth,
                a: 0.5 + 0.5 * depth,
            });
        }
    }
    finalize_frame(dots, Vec::new(), 0.3)
}

// 2. Globe (searching)
fn frame_globe(size: f32, t: f32, speed_mul: f32, count_mul: f32, size_mul: f32) -> OrbFrame {
    let cx = size / 2.0;
    let cy = size / 2.0;
    let r_sphere = (size / 2.0) * 0.82;
    let rs = radius_scale(size, 0.6) * size_mul;
    let time = t * speed_mul;

    let rt = count_mul.sqrt();
    let lat_rings = ((17.0 * rt).round() as usize).max(3);
    let lon_density = ((44.0 * rt).round() as usize).max(4);

    let m = time * 1.7;
    let cam_tilt = 0.4 + 0.06 * (time * 0.35).sin();

    let mut dots = Vec::new();
    for w in 0..=lat_rings {
        let lat =
            -std::f32::consts::FRAC_PI_2 + (w as f32 / lat_rings as f32) * std::f32::consts::PI;
        let cos_lat = lat.cos();
        let sin_lat = lat.sin();
        let lon_count = ((cos_lat.abs() * lon_density as f32).round() as usize).max(1);

        for f in 0..lon_count {
            let lon = (f as f32 / lon_count as f32) * 2.0 * std::f32::consts::PI;
            let nx = cos_lat * lon.cos();
            let ny = sin_lat;
            let nz = cos_lat * lon.sin();

            let (sx, sy, sz) = project_point(
                nx * r_sphere,
                ny * r_sphere,
                nz * r_sphere,
                time * 0.5,
                cam_tilt,
                cx,
                cy,
                1.0,
            );
            let depth = (sz / r_sphere + 1.0) / 2.0;
            let diff = lon + time * 0.5 - m;
            let k = diff.sin().atan2(diff.cos());
            let scan = (-k * k / 0.18).exp() * (sz / r_sphere).max(0.0);

            dots.push(Dot {
                x: sx,
                y: sy,
                z: sz,
                r: (0.6 + 1.7 * depth + 1.0 * scan) * rs,
                white: 0.62 - 0.54 * depth,
                a: 0.4 + 0.6 * scan.min(1.0),
            });
        }
    }
    finalize_frame(dots, Vec::new(), 0.3)
}

// 3. Rubik (solving)
struct Move {
    axis: usize,
    lo: f32,
    hi: f32,
    ang: f32,
}

fn make_moves(count: usize) -> Vec<Move> {
    let mut moves = Vec::with_capacity(count);
    for i in 0..count {
        let axis = (hash_d(i as f32, 2.3) * 3.0).floor().min(2.0) as usize;
        let lo = -1.0 + 0.5 * (hash_d(i as f32, 5.9) * 4.0).floor().min(3.0);
        let ang = if hash_d(i as f32, 7.7) < 0.5 {
            1.0
        } else {
            -1.0
        } * std::f32::consts::FRAC_PI_2;
        moves.push(Move {
            axis,
            lo,
            hi: lo + 0.5,
            ang,
        });
    }
    moves
}

fn solve_cycle(time: f32, count: usize, slot_dur: f32, rest: f32) -> (Vec<f32>, i32) {
    let cyc = 2.0 * count as f32 * slot_dur + rest;
    let tc = time.rem_euclid(cyc);
    let mut amount = vec![0.0; count];
    let mut active = -1;

    if tc < 2.0 * count as f32 * slot_dur {
        let slot = (tc / slot_dur).floor() as usize;
        let p = (tc - slot as f32 * slot_dur) / slot_dur;
        let cl = (p / 0.7).min(1.0);
        let ep = 1.0 - (1.0 - cl).powi(3);

        if slot < count {
            for i in 0..slot {
                amount[i] = 1.0;
            }
            amount[slot] = ep;
            active = slot as i32;
        } else {
            let u = 2 * count - 1 - slot;
            for i in 0..u {
                amount[i] = 1.0;
            }
            amount[u] = 1.0 - ep;
            active = u as i32;
        }
    }
    (amount, active)
}

fn apply_moves(
    mut pt: [f32; 3],
    moves: &[Move],
    amount: &[f32],
    active: i32,
) -> ([f32; 3], bool) {
    let mut in_active = false;
    for (i, mv) in moves.iter().enumerate() {
        if amount[i] <= 0.0 {
            continue;
        }
        let coord = pt[mv.axis];
        if coord < mv.lo || coord >= mv.hi {
            continue;
        }
        if i as i32 == active {
            in_active = true;
        }
        let a = mv.ang * amount[i];
        let ca = a.cos();
        let sa = a.sin();
        match mv.axis {
            0 => {
                let y2 = pt[1] * ca - pt[2] * sa;
                pt[2] = pt[1] * sa + pt[2] * ca;
                pt[1] = y2;
            }
            1 => {
                let x2 = pt[0] * ca + pt[2] * sa;
                pt[2] = -pt[0] * sa + pt[2] * ca;
                pt[0] = x2;
            }
            _ => {
                let x2 = pt[0] * ca - pt[1] * sa;
                pt[1] = pt[0] * sa + pt[1] * ca;
                pt[0] = x2;
            }
        }
    }
    (pt, in_active)
}

fn frame_rubik(size: f32, t: f32, speed_mul: f32, count_mul: f32, size_mul: f32) -> OrbFrame {
    let cx = size / 2.0;
    let cy = size / 2.0;
    let r_sphere = (size / 2.0) * 0.82;
    let rs = radius_scale(size, 0.6) * size_mul;
    let time = t * speed_mul;

    let rt = count_mul.sqrt();
    let lat_rings = ((15.0 * rt).round() as usize).max(3);
    let lon_density = ((40.0 * rt).round() as usize).max(4);

    let move_count = 14;
    let moves = make_moves(move_count);
    let (amount, active) = solve_cycle(time, move_count, 0.42, 1.2);
    let cam_tilt = 0.35 + 0.1 * (time * 0.9).sin();

    let mut dots = Vec::new();
    for w in 0..=lat_rings {
        let lat =
            -std::f32::consts::FRAC_PI_2 + (w as f32 / lat_rings as f32) * std::f32::consts::PI;
        let cos_lat = lat.cos();
        let sin_lat = lat.sin();
        let lon_count = ((cos_lat.abs() * lon_density as f32).round() as usize).max(1);

        for f in 0..lon_count {
            let lon = (f as f32 / lon_count as f32) * 2.0 * std::f32::consts::PI;
            let (p, in_active) = apply_moves(
                [cos_lat * lon.cos(), sin_lat, cos_lat * lon.sin()],
                &moves,
                &amount,
                active,
            );

            let (sx, sy, sz) = project_point(
                p[0] * r_sphere,
                p[1] * r_sphere,
                p[2] * r_sphere,
                time * 0.55,
                cam_tilt,
                cx,
                cy,
                1.0,
            );
            let depth = (sz / r_sphere + 1.0) / 2.0;

            dots.push(Dot {
                x: sx,
                y: sy,
                z: sz,
                r: (0.6 + 1.7 * depth + if in_active { 0.3 } else { 0.0 }) * rs,
                white: 0.62 - 0.54 * depth - if in_active { 0.14 } else { 0.0 },
                a: 0.9,
            });
        }
    }
    finalize_frame(dots, Vec::new(), 0.3)
}

// 4. Wave (listening)
fn frame_wave(size: f32, t: f32, speed_mul: f32, count_mul: f32, size_mul: f32) -> OrbFrame {
    let cx = size / 2.0;
    let cy = size / 2.0;
    let r_sphere = (size / 2.0) * 0.82;
    let rs = radius_scale(size, 0.6) * size_mul;
    let time = t * speed_mul;

    let rt = count_mul.sqrt();
    let lat_rings = ((16.0 * rt).round() as usize).max(3);
    let lon_density = ((42.0 * rt).round() as usize).max(4);

    let mut dots = Vec::new();
    for w in 0..=lat_rings {
        let lat =
            -std::f32::consts::FRAC_PI_2 + (w as f32 / lat_rings as f32) * std::f32::consts::PI;
        let cos_lat = lat.cos();
        let sin_lat = lat.sin();
        let lon_count = ((cos_lat.abs() * lon_density as f32).round() as usize).max(1);

        for f in 0..lon_count {
            let lon = (f as f32 / lon_count as f32) * 2.0 * std::f32::consts::PI;
            let wave = (lat * 3.0 + lon * 2.0 - time * 2.5).sin();
            let wave_amp = 0.08 * wave;
            let r_mod = r_sphere * (1.0 + wave_amp);

            let (sx, sy, sz) = project_point(
                cos_lat * lon.cos() * r_mod,
                sin_lat * r_mod,
                cos_lat * lon.sin() * r_mod,
                time * 0.4,
                0.32,
                cx,
                cy,
                1.0,
            );
            let depth = (sz / r_sphere + 1.0) / 2.0;
            let pulse = (wave * 0.5 + 0.5).clamp(0.0, 1.0);

            dots.push(Dot {
                x: sx,
                y: sy,
                z: sz,
                r: (0.7 + 1.5 * depth + 0.8 * pulse) * rs,
                white: 0.65 - 0.5 * depth,
                a: 0.4 + 0.6 * pulse,
            });
        }
    }
    finalize_frame(dots, Vec::new(), 0.3)
}

// 5. Web (connecting)
fn frame_web(size: f32, t: f32, speed_mul: f32, count_mul: f32, size_mul: f32) -> OrbFrame {
    let cx = size / 2.0;
    let cy = size / 2.0;
    let r_sphere = (size / 2.0) * 0.8;
    let rs = radius_scale(size, 0.6) * size_mul;
    let time = t * speed_mul;

    let node_n = ((30.0 * count_mul).round() as usize).max(6);
    let thr = 0.72;

    let mut raw_nodes = Vec::with_capacity(node_n);
    for i in 0..node_n {
        let d = fib_dir(i, node_n);
        let x = d[0] + 0.3 * (vnoise(i as f32 * 0.31 + 9.0, time * 0.24) - 0.5) * 2.0;
        let y = d[1] + 0.3 * (vnoise(i as f32 * 0.53 + 27.0, time * 0.21) - 0.5) * 2.0;
        let z = d[2] + 0.3 * (vnoise(i as f32 * 0.77 + 55.0, time * 0.27) - 0.5) * 2.0;
        let len = (x * x + y * y + z * z).sqrt().max(1e-6);
        raw_nodes.push([x / len, y / len, z / len]);
    }

    let mut projected = Vec::with_capacity(node_n);
    for n in &raw_nodes {
        let (sx, sy, sz) = project_point(
            n[0] * r_sphere,
            n[1] * r_sphere,
            n[2] * r_sphere,
            time * 0.12,
            0.32,
            cx,
            cy,
            1.0,
        );
        projected.push((sx, sy, sz));
    }

    let mut lines = Vec::new();
    let mut dots = Vec::new();

    for i in 0..node_n {
        let (sx1, sy1, sz1) = projected[i];
        let d1 = (sz1 / r_sphere + 1.0) / 2.0;

        for j in (i + 1)..node_n {
            let dx = raw_nodes[i][0] - raw_nodes[j][0];
            let dy = raw_nodes[i][1] - raw_nodes[j][1];
            let dz = raw_nodes[i][2] - raw_nodes[j][2];
            let dist = (dx * dx + dy * dy + dz * dz).sqrt();

            if dist < thr {
                let (sx2, sy2, sz2) = projected[j];
                let d2 = (sz2 / r_sphere + 1.0) / 2.0;
                let edge_depth = (d1 + d2) / 2.0;
                let proximity = 1.0 - dist / thr;

                lines.push(Line {
                    x1: sx1,
                    y1: sy1,
                    x2: sx2,
                    y2: sy2,
                    white: 0.6 - 0.4 * edge_depth,
                    a: 0.15 + 0.35 * proximity * edge_depth,
                    w: (0.8 * proximity * rs).max(0.5),
                });

                let sig_phase =
                    (time * 0.6 + hash_d(i as f32, j as f32) * 10.0).rem_euclid(1.0);
                let px = sx1 + (sx2 - sx1) * sig_phase;
                let py = sy1 + (sy2 - sy1) * sig_phase;
                let pz = sz1 + (sz2 - sz1) * sig_phase;
                dots.push(Dot {
                    x: px,
                    y: py,
                    z: pz,
                    r: 1.2 * rs,
                    white: 0.05,
                    a: 0.8 * proximity,
                });
            }
        }

        dots.push(Dot {
            x: sx1,
            y: sy1,
            z: sz1,
            r: (1.4 + 1.8 * d1) * rs,
            white: 0.5 - 0.45 * d1,
            a: 0.5 + 0.5 * d1,
        });
    }

    finalize_frame(dots, lines, 0.3)
}

// 6. Braid (weaving)
fn frame_braid(size: f32, t: f32, speed_mul: f32, count_mul: f32, size_mul: f32) -> OrbFrame {
    let cx = size / 2.0;
    let cy = size / 2.0;
    let r_sphere = (size / 2.0) * 0.76;
    let rs = radius_scale(size, 0.6) * size_mul;
    let time = t * speed_mul;

    let ghost_n = ((150.0 * count_mul).round() as usize).max(20);
    let strand_n = ((52.0 * count_mul).round() as usize).max(10);
    let turns = 3.0;

    let mut dots = Vec::new();
    for i in 0..ghost_n {
        let d = fib_dir(i, ghost_n);
        let (sx, sy, sz) = project_point(
            d[0] * r_sphere,
            d[1] * r_sphere,
            d[2] * r_sphere,
            time * 0.4,
            0.3,
            cx,
            cy,
            1.0,
        );
        let depth = (sz / r_sphere + 1.0) / 2.0;
        dots.push(Dot {
            x: sx,
            y: sy,
            z: sz,
            r: 0.8 * rs,
            white: 0.78,
            a: 0.1 + 0.22 * depth,
        });
    }

    for s in 0..3 {
        let strand_offset = s as f32 / 3.0 * 2.0 * std::f32::consts::PI;
        for i in 0..strand_n {
            let u =
                ((i as f32 / strand_n as f32 + time * 0.045).rem_euclid(1.0) * 2.0 - 1.0) * 0.96;
            let rad = (1.0 - u * u).max(0.0).sqrt();
            let fade = ((1.0 - u.abs()) / 0.1).min(1.0);

            let angle = u * std::f32::consts::PI * turns + strand_offset;
            let breathing = 1.0
                + 0.075
                    * (u * std::f32::consts::PI * turns * 2.0 + strand_offset * 2.0 + time * 0.8)
                        .sin();
            let f = rad * r_sphere * breathing;

            let px = angle.cos() * f;
            let py = u * r_sphere * breathing;
            let pz = angle.sin() * f;

            let (sx, sy, sz) =
                project_point(px, py, pz, time * 0.4, 0.3, cx, cy, 1.0);
            let depth = (sz / r_sphere + 1.0) / 2.0;

            dots.push(Dot {
                x: sx,
                y: sy,
                z: sz,
                r: (1.2 + 1.8 * depth) * rs,
                white: 0.55 - 0.45 * depth,
                a: fade * (0.45 + 0.55 * depth),
            });
        }
    }

    finalize_frame(dots, Vec::new(), 0.3)
}

// 7. Ribbon (composing)
fn frame_ribbon(size: f32, t: f32, speed_mul: f32, count_mul: f32, size_mul: f32) -> OrbFrame {
    let cx = size / 2.0;
    let cy = size / 2.0;
    let r_sphere = (size / 2.0) * 0.78;
    let rs = radius_scale(size, 0.6) * size_mul;
    let time = t * speed_mul;

    let ghost_n = ((150.0 * count_mul).round() as usize).max(20);
    let lanes = 5;
    let segs = ((60.0 * count_mul).round() as usize).max(12);

    let mut dots = Vec::new();
    for i in 0..ghost_n {
        let d = fib_dir(i, ghost_n);
        let (sx, sy, sz) = project_point(
            d[0] * r_sphere,
            d[1] * r_sphere,
            d[2] * r_sphere,
            time * 0.1,
            0.3,
            cx,
            cy,
            1.0,
        );
        let depth = (sz / r_sphere + 1.0) / 2.0;
        dots.push(Dot {
            x: sx,
            y: sy,
            z: sz,
            r: 0.8 * rs,
            white: 0.78,
            a: 0.1 + 0.22 * depth,
        });
    }

    let ya = time * 0.24;
    let ta = 0.55 + 0.3 * (time * 0.18).sin();
    let ux = ya.cos();
    let uy = 0.0;
    let uz = ya.sin();
    let vx = -uz * ta.sin();
    let vy = ta.cos();
    let vz = ux * ta.sin();

    for l in 0..lanes {
        let lane_offset = (l as f32 - (lanes - 1) as f32 / 2.0) * 0.06;
        for s in 0..segs {
            let a = (s as f32 / segs as f32) * 2.0 * std::f32::consts::PI;
            let undulation = 1.0 + 0.08 * (a * 4.0 + time * 1.5).sin();
            let ro = r_sphere * undulation;

            let bx = (ux * a.cos() + vx * a.sin()) * ro;
            let by = (uy * a.cos() + vy * a.sin()) * ro;
            let bz = (uz * a.cos() + vz * a.sin()) * ro;

            let px = bx + vx * lane_offset * r_sphere;
            let py = by + vy * lane_offset * r_sphere;
            let pz = bz + vz * lane_offset * r_sphere;

            let (sx, sy, sz) =
                project_point(px, py, pz, time * 0.1, 0.3, cx, cy, 1.0);
            let depth = (sz / r_sphere + 1.0) / 2.0;

            dots.push(Dot {
                x: sx,
                y: sy,
                z: sz,
                r: (1.1 + 1.6 * depth) * rs,
                white: 0.55 - 0.45 * depth,
                a: 0.4 + 0.6 * depth,
            });
        }
    }

    finalize_frame(dots, Vec::new(), 0.3)
}

// 8. Ring (breathing)
fn frame_ring(size: f32, t: f32, speed_mul: f32, count_mul: f32, size_mul: f32) -> OrbFrame {
    let cx = size / 2.0;
    let cy = size / 2.0;
    let r_sphere = (size / 2.0) * 0.78;
    let rs = radius_scale(size, 0.6) * size_mul;
    let time = t * speed_mul;

    let ghost_n = ((150.0 * count_mul).round() as usize).max(20);
    let lanes = 4;
    let segs = ((60.0 * count_mul).round() as usize).max(12);

    let mut dots = Vec::new();
    for i in 0..ghost_n {
        let d = fib_dir(i, ghost_n);
        let (sx, sy, sz) = project_point(
            d[0] * r_sphere,
            d[1] * r_sphere,
            d[2] * r_sphere,
            time * 0.05,
            0.0,
            cx,
            cy,
            1.0,
        );
        let depth = (sz / r_sphere + 1.0) / 2.0;
        dots.push(Dot {
            x: sx,
            y: sy,
            z: sz,
            r: 0.8 * rs,
            white: 0.78,
            a: 0.08 + 0.18 * depth,
        });
    }

    for l in 0..lanes {
        let lane_r = (l as f32 - (lanes - 1) as f32 / 2.0) * 0.04 * r_sphere;
        for s in 0..segs {
            let a = (s as f32 / segs as f32) * 2.0 * std::f32::consts::PI;
            let breathing = (time * 0.8).sin() * 0.08 + (a * 3.0 + time * 1.2).sin() * 0.04;
            let r_curr = r_sphere * (0.85 + breathing) + lane_r;

            let px = a.cos() * r_curr;
            let py = a.sin() * r_curr;
            let pz = (a * 2.0 + time).sin() * 0.1 * r_sphere;

            let (sx, sy, sz) = project_point(px, py, pz, 0.0, 0.0, cx, cy, 1.0);
            let depth = (sz / r_sphere + 1.0) / 2.0;

            dots.push(Dot {
                x: sx,
                y: sy,
                z: sz,
                r: (1.2 + 1.5 * depth) * rs,
                white: 0.5 - 0.4 * depth,
                a: 0.5 + 0.5 * depth,
            });
        }
    }

    finalize_frame(dots, Vec::new(), 0.3)
}

// 9. Morph (shaping)
type Point2D = [f32; 2];

fn poly_path(verts: &[Point2D], f: f32) -> Point2D {
    let v_len = verts.len();
    let mut lengths = Vec::with_capacity(v_len);
    let mut total = 0.0;
    for i in 0..v_len {
        let a = verts[i];
        let b = verts[(i + 1) % v_len];
        let l = (b[0] - a[0]).hypot(b[1] - a[1]);
        lengths.push(l);
        total += l;
    }
    let mut target = f.rem_euclid(1.0) * total;
    let mut i = 0;
    while i < v_len - 1 && target > lengths[i] {
        target -= lengths[i];
        i += 1;
    }
    let a = verts[i];
    let b = verts[(i + 1) % v_len];
    let ff = if lengths[i] > 0.0 {
        (target / lengths[i]).min(1.0)
    } else {
        0.0
    };
    [a[0] + (b[0] - a[0]) * ff, a[1] + (b[1] - a[1]) * ff]
}

fn circle_path(f: f32) -> Point2D {
    let a = -std::f32::consts::FRAC_PI_2 + f * 2.0 * std::f32::consts::PI;
    [a.cos() * 0.24, a.sin() * 0.24]
}

fn triangle_path(f: f32) -> Point2D {
    let verts = [[0.0, -0.26], [0.24, 0.16], [-0.24, 0.16]];
    poly_path(&verts, f)
}

fn square_path(f: f32) -> Point2D {
    let verts = [
        [0.0, -0.2],
        [0.2, -0.2],
        [0.2, 0.2],
        [-0.2, 0.2],
        [-0.2, -0.2],
    ];
    poly_path(&verts, f)
}

fn frame_morph(size: f32, t: f32, speed_mul: f32, count_mul: f32, size_mul: f32) -> OrbFrame {
    let cx = size / 2.0;
    let cy = size / 2.0;
    let rs = radius_scale(size, 0.6) * size_mul;
    let time = t * speed_mul * 0.35;

    let dot_count = ((48.0 * count_mul).round() as usize).max(12);
    let period = time.rem_euclid(3.0);
    let stage = period.floor() as usize;
    let frac = period - stage as f32;
    let ease = frac * frac * (3.0 - 2.0 * frac);

    let path_a: fn(f32) -> Point2D = match stage {
        0 => circle_path,
        1 => triangle_path,
        _ => square_path,
    };
    let path_b: fn(f32) -> Point2D = match stage {
        0 => triangle_path,
        1 => square_path,
        _ => circle_path,
    };

    let scale = size * 1.5;
    let mut dots = Vec::with_capacity(dot_count);
    for i in 0..dot_count {
        let f = i as f32 / dot_count as f32;
        let p_a = path_a(f);
        let p_b = path_b(f);
        let px = cx + (p_a[0] + (p_b[0] - p_a[0]) * ease) * scale;
        let py = cy + (p_a[1] + (p_b[1] - p_a[1]) * ease) * scale;

        dots.push(Dot {
            x: px,
            y: py,
            z: 0.0,
            r: 1.4 * rs,
            white: 0.1,
            a: 0.9,
        });
    }

    finalize_frame(dots, Vec::new(), 0.3)
}

// ---------------------------------------------------------------------------
// ThinkingOrb GPUI Component
// ---------------------------------------------------------------------------

static ORB_EPOCH: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

/// Returns the continuous elapsed time in seconds for the Thinking Orb animations.
pub fn orb_time() -> f32 {
    ORB_EPOCH.elapsed().as_secs_f32()
}

/// The ThinkingOrb GPUI Component.
#[derive(Clone)]
pub struct ThinkingOrb {
    pub id: Option<&'static str>,
    pub state: OrbState,
    pub size: f32,
    pub speed: f32,
    pub paused: bool,
    pub dark_override: Option<bool>,
    pub driven_view: Option<gpui::EntityId>,
}

impl ThinkingOrb {
    pub fn new(state: OrbState, size: f32) -> Self {
        Self {
            id: None,
            state,
            size,
            speed: 1.0,
            paused: false,
            dark_override: None,
            driven_view: None,
        }
    }

    pub fn id(mut self, id: &'static str) -> Self {
        self.id = Some(id);
        self
    }

    pub fn state(mut self, state: OrbState) -> Self {
        self.state = state;
        self
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub fn speed(mut self, speed: f32) -> Self {
        self.speed = speed;
        self
    }

    pub fn paused(mut self, paused: bool) -> Self {
        self.paused = paused;
        self
    }

    pub fn dark(mut self, dark: bool) -> Self {
        self.dark_override = Some(dark);
        self
    }

    /// Lease redraw notifications for the parent view while this orb is rendered.
    pub fn driven(mut self, view: gpui::EntityId, cx: &mut gpui::App) -> Self {
        crate::motion::pulse_lease(view, cx);
        self.driven_view = Some(view);
        self
    }
}

impl IntoElement for ThinkingOrb {
    type Element = AnyElement;

    fn into_element(self) -> Self::Element {
        let size_px = self.size;
        let state = self.state;
        let speed = self.speed;
        let paused = self.paused;
        let dark_override = self.dark_override;
        let driven_view = self.driven_view;

        div()
            .relative()
            .size(px(size_px))
            .flex_none()
            .child(
                canvas(
                    move |_bounds, _window, _cx| (),
                    move |bounds, _, window, cx| {
                        let theme = Theme::of(cx);
                        let is_dark = dark_override.unwrap_or(theme.appearance.is_dark());

                        // Clock drive with reduced-motion support.
                        let reduced = cx.reduce_motion();
                        let time = if reduced || paused {
                            0.0
                        } else {
                            if let Some(v) = driven_view {
                                crate::motion::pulse_lease(v, cx);
                            }
                            window.request_animation_frame();
                            orb_time()
                        };

                        let frame = generate_frame(state, size_px, time, speed);

                        let origin_x = f32::from(bounds.origin.x);
                        let origin_y = f32::from(bounds.origin.y);

                        // 1. Draw Lines (for `connecting` constellation)
                        for line in frame.lines {
                            let mut path = PathBuilder::stroke(px(line.w));
                            path.move_to(point(px(origin_x + line.x1), px(origin_y + line.y1)));
                            path.line_to(point(px(origin_x + line.x2), px(origin_y + line.y2)));

                            let ink = line.white.clamp(0.0, 1.0);
                            let val = if is_dark { 1.0 - ink } else { ink };
                            let color: gpui::Hsla = gpui::rgba(
                                ((val * 255.0).round() as u32) * 0x01010100
                                    | ((line.a * 255.0).round() as u32).min(255),
                            )
                            .into();

                            if let Ok(built) = path.build() {
                                window.paint_path(built, color);
                            }
                        }

                        // 2. Draw Dots (z-sorted circles painted via GPU quads)
                        for dot in frame.dots {
                            let r = dot.r;
                            let center_x = origin_x + dot.x;
                            let center_y = origin_y + dot.y;

                            let rect = Bounds {
                                origin: point(px(center_x - r), px(center_y - r)),
                                size: size(px(r * 2.0), px(r * 2.0)),
                            };

                            let ink = dot.white.clamp(0.0, 1.0);
                            let val = if is_dark { 1.0 - ink } else { ink };
                            let alpha = dot.a.clamp(0.0, 1.0);

                            let lum = val;
                            let color = gpui::hsla(0.0, 0.0, lum, alpha);

                            window.paint_quad(quad(
                                rect,
                                px(r),
                                color,
                                px(0.0),
                                gpui::transparent_black(),
                                BorderStyle::default(),
                            ));
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .into_any_element()
    }
}

/// Convenience helper to create a ThinkingOrb element.
pub fn thinking_orb(state: OrbState, size_px: f32) -> ThinkingOrb {
    ThinkingOrb::new(state, size_px)
}
