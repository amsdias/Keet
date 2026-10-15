//! Cymatics: sand on a vibrating plate, as in Chladni's experiment. A plate
//! driven at one of its resonances stands still along some lines and moves
//! everywhere else, so sand shaken off the moving parts gathers on the still
//! ones and draws the mode's pattern.
//!
//! The plate is 1, 2 or 3 times as wide as it is tall, whichever fills the
//! window best. On a plate `a` wide and 1 tall (edges free), the simple
//! vibration `cos(pπx)·cos(qπy)` (x and y across 0..1) rings at a frequency
//! that goes with p² + a²q²; two of them that ring at the SAME frequency,
//! (p, q) and (r, s), combine into the patterns sand draws:
//! `cos(pπx)·cos(qπy) − cos(rπx)·cos(sπy)`. On the square that is Chladni's
//! classic (n, m) / (m, n) pair; on the wide plates some pairs repeat a
//! square pattern mirrored across the width and others exist only there.
//! The sand lies where that is zero: within a fixed distance of the line, measured
//! as the value over its slope, so every line is as thin as every other
//! (the value alone stays small over whole patches where the plate barely
//! moves, which drew blobs instead of lines).
//!
//! Each plate's 31 simplest patterns, in the order of the frequency they ring at, are
//! matched one to one with the 31 bands, so a low note draws a few broad
//! lines and a high one a dense lattice. The loudest band picks the mode;
//! a change waits until the new band has stayed loudest a moment (or is
//! clearly louder), and the plate then MORPHS: the old pattern blends into
//! the new one, the sand sliding into place. Louder music puts more sand on
//! the plate and makes it shimmer; silence thins it and leaves the last
//! pattern standing.
//!
//! The music's part ([`CymaticsMotion`]) advances in the analyser's
//! per-frame step by the audio it covered; drawing ([`render_cymatics`]) is
//! a pure function of it and the wall-clock time (for the shimmer).

use crate::state::{VizStyle, SPECTRUM_BANDS};
use crate::theme::{base_hue, hsv, Rgb, ThemeKind};
use crate::tui::LineBuf;
use crate::viz::{fit_width, Lines, BRAILLE_BITS, ISO_CENTERS};
use ratatui::text::Line;
use std::f32::consts::PI;

/// The full-window ceiling: past this the plate is only drawn bigger.
pub const CYMATICS_MAX_ROWS: usize = 60;

/// A band must be at least this loud (0..=1) to set the plate's mode;
/// below it the plate keeps the pattern it has.
const QUIET: f32 = 0.3;
/// How long a new band must stay the loudest before the plate follows it
/// (seconds), or how much louder than the current band it must be to be
/// followed at once.
const HOLD: f32 = 0.12;
const CLEARLY_LOUDER: f32 = 0.08;
/// The morph's time constant (seconds): about 95% of the way in three.
const MORPH_TAU: f32 = 0.15;
/// Patterns blending at once; past this the faintest is folded into the
/// newest.
const MAX_PARTS: usize = 4;

/// One pattern: the two vibrations (p, q) and (r, s) that ring together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Mode {
    p: u8,
    q: u8,
    r: u8,
    s: u8,
}

/// The widest plate drawn (width : height).
const MAX_ASPECT: usize = 3;

/// The patterns of the plate `aspect` times as wide as tall, simplest
/// (lowest-ringing) first, one per band: every pair of vibrations (p, q),
/// (r, s), all ≥ 1, with p² + a²q² = r² + a²s², each pair once.
fn modes(aspect: usize) -> &'static [Mode; SPECTRUM_BANDS] {
    static MODES: [std::sync::OnceLock<[Mode; SPECTRUM_BANDS]>; MAX_ASPECT] =
        [std::sync::OnceLock::new(), std::sync::OnceLock::new(), std::sync::OnceLock::new()];
    let a = aspect.clamp(1, MAX_ASPECT);
    MODES[a - 1].get_or_init(|| {
        let a2 = (a * a) as u32;
        let freq = |p: u8, q: u8| p as u32 * p as u32 + a2 * q as u32 * q as u32;
        let mut by_freq: std::collections::BTreeMap<u32, Vec<(u8, u8)>> = Default::default();
        for p in 1..=(10 * a as u8) {
            for q in 1..=10u8 {
                by_freq.entry(freq(p, q)).or_default().push((p, q));
            }
        }
        let mut all = Vec::new();
        for same in by_freq.values() {
            for (i, &(p, q)) in same.iter().enumerate() {
                for &(r, s) in &same[i + 1..] {
                    all.push(Mode { p, q, r, s });
                }
            }
        }
        std::array::from_fn(|i| all[i])
    })
}

/// The plate shape (width : height) that shows the most of it in `width`
/// columns by `rows`: its side in rows, and the aspect. Wider wins only
/// when it is bigger, a tie going to the squarer plate.
fn plate_shape(width: usize, rows: usize) -> (usize, usize) {
    (1..=MAX_ASPECT)
        .map(|a| (rows.min(width.saturating_sub(2) / (2 * a)), a))
        .fold((0, 1), |best, (side, a)| if side * side * a > best.0 * best.0 * best.1 { (side, a) } else { best })
}

/// What the music has done to the plate.
#[derive(Clone, Debug)]
pub struct CymaticsMotion {
    /// The patterns blending on the plate: (band, weight), weights summing
    /// to 1; the target is among them.
    parts: Vec<(usize, f32)>,
    /// The band the plate is heading for.
    target: usize,
    /// A band that has been the loudest, and for how long (seconds).
    candidate: Option<(usize, f32)>,
    /// Loudness, smoothed (0..=1): how much sand there is.
    level: f32,
}

impl Default for CymaticsMotion {
    fn default() -> Self {
        // A simple pattern to start from: the plate's fourth mode.
        CymaticsMotion { parts: vec![(3, 1.0)], target: 3, candidate: None, level: 0.0 }
    }
}

impl CymaticsMotion {
    /// Advance by `dt` seconds of audio with `bands` the spectrum's (smoothed)
    /// band levels, both channels together.
    pub fn update(&mut self, dt: f32, bands: &[f32; SPECTRUM_BANDS]) {
        if dt <= 0.0 {
            return;
        }
        let (best, loudest) = bands
            .iter()
            .copied()
            .enumerate()
            .fold((0, f32::MIN), |acc, (i, v)| if v > acc.1 { (i, v) } else { acc });
        self.level += (loudest.clamp(0.0, 1.0) - self.level) * (1.0 - (-dt / 0.2).exp());
        if loudest >= QUIET && best != self.target {
            let held = match self.candidate {
                Some((b, h)) if b == best => h + dt,
                _ => dt,
            };
            self.candidate = Some((best, held));
            if held >= HOLD || loudest > bands[self.target] + CLEARLY_LOUDER {
                self.retarget(best);
            }
        } else if best == self.target {
            self.candidate = None;
        }
        self.morph(dt);
    }

    /// Head for `band`'s pattern, from whatever blend is on the plate now.
    fn retarget(&mut self, band: usize) {
        self.target = band;
        self.candidate = None;
        if !self.parts.iter().any(|&(b, _)| b == band) {
            self.parts.push((band, 0.0));
        }
        while self.parts.len() > MAX_PARTS {
            let faintest = (0..self.parts.len())
                .filter(|&i| self.parts[i].0 != band)
                .min_by(|&a, &b| self.parts[a].1.total_cmp(&self.parts[b].1))
                .expect("more parts than one");
            let (_, w) = self.parts.remove(faintest);
            if let Some(t) = self.parts.iter_mut().find(|p| p.0 == band) {
                t.1 += w;
            }
        }
    }

    /// Move the blend `dt` seconds toward the target: every other pattern
    /// fades by the same share, and the target takes what they give up.
    fn morph(&mut self, dt: f32) {
        let keep = (-dt / MORPH_TAU).exp();
        let target = self.target;
        let mut given = 0.0;
        for (band, w) in &mut self.parts {
            if *band != target {
                given += *w * (1.0 - keep);
                *w *= keep;
            }
        }
        // A pattern all but gone leaves the plate.
        self.parts.retain(|&(b, w)| b == target || w >= 0.005);
        let others: f32 = self.parts.iter().filter(|p| p.0 != target).map(|p| p.1).sum();
        if let Some(t) = self.parts.iter_mut().find(|p| p.0 == target) {
            t.1 = (1.0 - others).max(t.1 + given).min(1.0);
        }
    }
}

/// One vibration along an axis, for each of `steps` positions across
/// the plate (dot centres): `cos(kπu)` and its slope, `−kπ·sin(kπu)`.
fn axis(k: u8, steps: usize) -> (Vec<f32>, Vec<f32>) {
    let w = k as f32 * PI;
    (0..steps)
        .map(|i| {
            let u = (i as f32 + 0.5) / steps as f32;
            ((w * u).cos(), -w * (w * u).sin())
        })
        .unzip()
}

/// One pattern on the plate: its weight, and its two vibrations' tables
/// (p and r across, q and s down).
struct Part {
    w: f32,
    xp: (Vec<f32>, Vec<f32>),
    xr: (Vec<f32>, Vec<f32>),
    yq: (Vec<f32>, Vec<f32>),
    ys: (Vec<f32>, Vec<f32>),
}

/// Seconds for the plate's colours to go once round the hue wheel.
const CYCLE_SECS: f32 = 30.0;
/// Degrees the hue moves across the plate, centre to corner.
const RADIAL_HUE: f32 = 80.0;

/// The plate's hue at its centre: the theme's colour, moved along by the
/// frequency (`freq`, 0 = the lowest band, 1 = the highest), warmed by
/// loudness (`level`), and cycling once round the wheel every CYCLE_SECS.
fn plate_hue(kind: ThemeKind, freq: f32, level: f32, t: f32) -> f32 {
    (base_hue(kind) + 120.0 * freq - 50.0 * level + 360.0 * t / CYCLE_SECS).rem_euclid(360.0)
}

/// Deterministic hash, for the sand's shimmer.
fn hash(x: i32, y: i32, salt: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(374_761_393)
        ^ (y as u32).wrapping_mul(668_265_263)
        ^ salt.wrapping_mul(2_246_822_519);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^ (h >> 16)
}

/// Wall-clock seconds since the first cymatics frame of this run.
fn wall_time() -> f32 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f32()
}

/// See [`cymatics_body`]; this enforces the width half of the renderer
/// contract (`fit_width`).
pub fn render_cymatics(motion: &CymaticsMotion, kind: ThemeKind, style: VizStyle, width: usize, rows: usize, extras: bool) -> Lines {
    fit_width(cymatics_body(motion, kind, style, width, rows, extras, wall_time()), width)
}

/// The readout `Shift+L` adds under the plate: the frequency the plate is
/// ringing at and its mode.
fn readout(motion: &CymaticsMotion, aspect: usize) -> String {
    let hz = ISO_CENTERS[motion.target];
    let Mode { p: n, q: m, .. } = modes(aspect)[motion.target];
    let freq = if hz < 1000.0 { format!("{hz:.0} Hz") } else { format!("{:.1} kHz", hz / 1000.0).replace(".0 ", " ") };
    if motion.level < QUIET * 0.5 {
        format!("≈ {freq} · mode {n}·{m} · quiet")
    } else {
        format!("≈ {freq} · mode {n}·{m}")
    }
}

/// Exactly `rows` lines of the plate `t` seconds in, none wider than `width`.
fn cymatics_body(motion: &CymaticsMotion, kind: ThemeKind, style: VizStyle, width: usize, rows: usize, extras: bool, t: f32) -> Lines {
    let legend = extras && rows >= 4;
    let plate_rows = rows - usize::from(legend);
    // `side` rows by 2·aspect times as many columns: a square cell pair is
    // one square on screen.
    let (side, aspect) = plate_shape(width, plate_rows);
    if side < 2 {
        return vec![Line::default(); rows];
    }
    let cols = side * 2 * aspect;
    let (dw, dh) = (cols * 2, side * 4);
    let parts: Vec<Part> = motion
        .parts
        .iter()
        .map(|&(band, w)| {
            let Mode { p, q, r, s } = modes(aspect)[band];
            Part { w, xp: axis(p, dw), xr: axis(r, dw), yq: axis(q, dh), ys: axis(s, dh) }
        })
        .collect();
    // How far (in dots) the dot at (gx, gy) is from the nearest still line:
    // the blended field over its slope per dot (the plate spans `dw` dots
    // across and `dh` down, and dots are square).
    let distance = |gx: usize, gy: usize| -> f32 {
        let (mut f, mut fx, mut fy) = (0.0, 0.0, 0.0);
        for p in &parts {
            f += p.w * (p.xp.0[gx] * p.yq.0[gy] - p.xr.0[gx] * p.ys.0[gy]);
            fx += p.w * (p.xp.1[gx] * p.yq.0[gy] - p.xr.1[gx] * p.ys.0[gy]);
            fy += p.w * (p.xp.0[gx] * p.yq.1[gy] - p.xr.0[gx] * p.ys.1[gy]);
        }
        let slope = ((fx / dw as f32).powi(2) + (fy / dh as f32).powi(2)).sqrt().max(1e-5);
        f.abs() / slope
    };
    // More sand the louder it is (lines up to about three dots wide); it
    // shimmers once the music is up.
    let eps = 0.55 + 1.1 * motion.level;
    let slot = (t * 12.0) as u32;
    let shimmer = motion.level > QUIET;
    // The colour: see plate_hue.
    let blended: f32 = motion.parts.iter().map(|&(b, w)| b as f32 * w).sum();
    let hue = plate_hue(kind, blended / (SPECTRUM_BANDS - 1) as f32, motion.level, t);
    // Across the plate the hue runs RADIAL_HUE from the centre to the
    // corners, so a frame holds a spread of colours, not one.
    let (half_w, half_h) = (cols as f32 / 2.0, side as f32 / 2.0);
    let colour = |closeness: f32, cx: usize, cy: usize| -> Rgb {
        let (dx, dy) = ((cx as f32 + 0.5 - half_w) / half_w, (cy as f32 + 0.5 - half_h) / half_h);
        let out = ((dx * dx + dy * dy) / 2.0).sqrt();
        hsv(
            hue + RADIAL_HUE * out,
            (0.75 - 0.35 * closeness).max(0.25),
            ((0.4 + 0.6 * closeness) * (0.6 + 0.4 * motion.level)).min(1.0),
        )
    };

    let x0 = (width - cols) / 2;
    let mut lines = Vec::with_capacity(rows);
    for _ in 0..(plate_rows - side) / 2 {
        lines.push(Line::default());
    }
    for cy in 0..side {
        let mut line = LineBuf::new();
        line.spaces(x0);
        for cx in 0..cols {
            let mut bits = 0u32;
            let mut nearest = f32::MAX;
            for (px, column) in BRAILLE_BITS.iter().enumerate() {
                for (py, bit) in column.iter().enumerate() {
                    let (gx, gy) = (cx * 2 + px, cy * 4 + py);
                    let f = distance(gx, gy);
                    nearest = nearest.min(f);
                    let jitter = if shimmer {
                        0.7 + 0.6 * (hash(gx as i32, gy as i32, slot) & 0xff) as f32 / 255.0
                    } else {
                        1.0
                    };
                    if f < eps * jitter {
                        bits |= bit;
                    }
                }
            }
            let closeness = (1.0 - nearest / (eps * 1.6)).clamp(0.0, 1.0);
            let glyph = match style {
                VizStyle::Dots => (bits != 0).then(|| char::from_u32(0x2800 + bits).unwrap_or(' ')),
                VizStyle::Bars => match closeness {
                    c if c > 0.75 => Some('█'),
                    c if c > 0.5 => Some('▓'),
                    c if c > 0.25 => Some('▒'),
                    c if c > 0.0 => Some('░'),
                    _ => None,
                },
            };
            match glyph {
                Some(ch) => line.fg(colour(closeness, cx, cy)).push(ch),
                None => line.reset().push(' '),
            };
        }
        let mut line = line.into_line();
        while line.spans.last().is_some_and(|s| s.content.trim().is_empty()) {
            line.spans.pop();
        }
        lines.push(line);
    }
    lines.resize(plate_rows, Line::default());
    if legend {
        let text = crate::ansi::truncate_plain(&readout(motion, aspect), width);
        let pad = width.saturating_sub(crate::tui::width(&text)) / 2;
        let mut line = LineBuf::new();
        line.spaces(pad).styled(&text, ratatui::style::Style::new().add_modifier(ratatui::style::Modifier::DIM));
        lines.push(line.into_line());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::plain as strip_ansi;

    fn visible_len(l: &Line) -> usize {
        crate::tui::Cols::cols(l)
    }

    fn lit(lines: &[Line]) -> u32 {
        lines
            .iter()
            .map(|l| strip_ansi(l).chars().map(|c| match c {
                '\u{2801}'..='\u{28FF}' => (c as u32 - 0x2800).count_ones(),
                '░' | '▒' | '▓' | '█' => 1,
                _ => 0,
            }).sum::<u32>())
            .sum()
    }

    /// `band` loudest at `level`, the rest well under it, for `secs`.
    fn play(m: &mut CymaticsMotion, band: usize, level: f32, secs: f32) {
        let mut bands = [level * 0.5; SPECTRUM_BANDS];
        bands[band] = level;
        let mut t = 0.0;
        while t < secs {
            m.update(0.05, &bands);
            t += 0.05;
        }
    }

    fn weight(m: &CymaticsMotion, band: usize) -> f32 {
        m.parts.iter().find(|p| p.0 == band).map_or(0.0, |p| p.1)
    }

    /// Exactly the rows given, none wider than the window, at every size.
    #[test]
    fn every_size_fills_its_rows_and_fits_the_width() {
        let mut m = CymaticsMotion::default();
        play(&mut m, 12, 0.8, 0.1); // mid-morph: two patterns on the plate
        for style in [VizStyle::Bars, VizStyle::Dots] {
            for extras in [false, true] {
                for w in (0..=40).chain([80, 133, 200]) {
                    for rows in 0..=CYMATICS_MAX_ROWS {
                        let lines = cymatics_body(&m, ThemeKind::HiFi, style, w, rows, extras, 2.0);
                        assert_eq!(lines.len(), rows, "{style:?} {extras} {w}x{rows}");
                        for l in &lines {
                            assert!(visible_len(l) <= w, "{style:?} {extras} {w}x{rows}: {l:?}");
                        }
                    }
                }
            }
        }
    }

    /// One pattern per band on every plate, all different, each a pair of
    /// vibrations ringing together, simplest (lowest-ringing) first.
    #[test]
    fn the_modes_go_from_simple_to_intricate() {
        for a in 1..=MAX_ASPECT {
            let ms = modes(a);
            let set: std::collections::HashSet<_> = ms.iter().collect();
            assert_eq!(set.len(), SPECTRUM_BANDS, "aspect {a}");
            let a2 = (a * a) as u32;
            let freq = |p: u8, q: u8| p as u32 * p as u32 + a2 * q as u32 * q as u32;
            for m in ms {
                assert_eq!(freq(m.p, m.q), freq(m.r, m.s), "aspect {a}: {m:?} is not one frequency");
                assert_ne!((m.p, m.q), (m.r, m.s));
            }
            assert!(ms.windows(2).all(|w| freq(w[0].p, w[0].q) <= freq(w[1].p, w[1].q)), "aspect {a}: {ms:?}");
        }
        // The square's first is Chladni's (2, 1) / (1, 2).
        assert_eq!(modes(1)[0], Mode { p: 1, q: 2, r: 2, s: 1 });
        // A high band draws a busier plate than a low one.
        let at = |band| CymaticsMotion { parts: vec![(band, 1.0)], target: band, candidate: None, level: 0.7 };
        let low = lit(&cymatics_body(&at(0), ThemeKind::Classic, VizStyle::Dots, 100, 30, false, 0.0));
        let high = lit(&cymatics_body(&at(30), ThemeKind::Classic, VizStyle::Dots, 100, 30, false, 0.0));
        assert!(high > low, "{high} vs {low}");
    }

    /// A band loudest for a moment does not move the plate; one that stays
    /// loudest does, and one clearly louder does at once.
    #[test]
    fn the_plate_follows_the_loudest_band_without_flicker() {
        let mut m = CymaticsMotion::default();
        play(&mut m, 3, 0.8, 1.0);
        let mut blip = [0.5; SPECTRUM_BANDS];
        blip[3] = 0.8;
        blip[20] = 0.82; // loudest for one frame, barely
        m.update(0.05, &blip);
        assert_eq!(m.target, 3, "a blip is ignored");
        play(&mut m, 20, 0.8, 0.2);
        assert_eq!(m.target, 20, "a band that stays loudest is followed");
        let mut jump = [0.4; SPECTRUM_BANDS];
        jump[20] = 0.5;
        jump[8] = 0.9;
        m.update(0.05, &jump);
        assert_eq!(m.target, 8, "a clearly louder band is followed at once");
        // Silence keeps the pattern.
        play(&mut m, 25, 0.1, 2.0);
        assert_eq!(m.target, 8);
    }

    /// A change morphs: halfway the plate is neither pattern, then it settles
    /// on the new one; weights always sum to one.
    #[test]
    fn a_change_morphs_from_one_pattern_to_the_next() {
        let mut m = CymaticsMotion::default();
        play(&mut m, 3, 0.8, 1.0);
        let draw = |m: &CymaticsMotion| cymatics_body(m, ThemeKind::Minimal, VizStyle::Dots, 80, 20, false, 0.0);
        let before = draw(&m);
        play(&mut m, 14, 0.8, 0.2);
        let total: f32 = m.parts.iter().map(|p| p.1).sum();
        assert!((total - 1.0).abs() < 1e-3, "{:?}", m.parts);
        assert!(weight(&m, 14) > 0.2 && weight(&m, 3) > 0.1, "mid-morph: {:?}", m.parts);
        let middle = draw(&m);
        play(&mut m, 14, 0.8, 1.5);
        let after = draw(&m);
        assert!(weight(&m, 14) > 0.99, "settled: {:?}", m.parts);
        assert!(middle != before && middle != after);
        // A change mid-morph heads for the newest from wherever it is.
        play(&mut m, 22, 0.8, 0.2);
        play(&mut m, 6, 0.8, 0.2);
        assert_eq!(m.target, 6);
        assert!(m.parts.len() <= MAX_PARTS);
        let total: f32 = m.parts.iter().map(|p| p.1).sum();
        assert!((total - 1.0).abs() < 1e-3, "{:?}", m.parts);
    }

    /// The plate takes the shape that fills the window: square in a square
    /// space, 2:1 or 3:1 when the window is that much wider than tall.
    #[test]
    fn the_plate_widens_with_the_window() {
        assert_eq!(plate_shape(60, 30).1, 1);
        assert_eq!(plate_shape(130, 30).1, 2);
        assert_eq!(plate_shape(200, 30).1, 3);
        assert_eq!(plate_shape(200, 30).0, 30, "as tall as the rows");
        // And it is drawn that wide.
        let m = CymaticsMotion { parts: vec![(12, 1.0)], target: 12, candidate: None, level: 0.8 };
        for (w, rows, a) in [(60, 30, 1), (130, 30, 2), (200, 30, 3)] {
            let lines = cymatics_body(&m, ThemeKind::Classic, VizStyle::Bars, w, rows, false, 0.0);
            let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
            let first = plain.iter().filter_map(|l| l.find(|c: char| c != ' ')).min().unwrap();
            let last = plain.iter().map(|l| l.chars().count()).max().unwrap();
            let span = last - first;
            let want = rows * 2 * a;
            assert!(span + 4 >= want && span <= want, "{w}x{rows}: {span} columns, want {want}");
        }
    }

    /// The colours cycle with time, move with the frequency and loudness,
    /// and spread across the plate: a frame has many colours.
    #[test]
    fn the_plate_is_coloured_and_its_colours_cycle() {
        let k = ThemeKind::Minimal;
        assert_ne!(plate_hue(k, 0.3, 0.5, 0.0), plate_hue(k, 0.3, 0.5, CYCLE_SECS / 2.0), "it cycles");
        assert!((plate_hue(k, 0.3, 0.5, 1.0) - plate_hue(k, 0.3, 0.5, 1.0 + CYCLE_SECS)).abs() < 0.01, "once a cycle");
        assert_ne!(plate_hue(k, 0.1, 0.5, 0.0), plate_hue(k, 0.9, 0.5, 0.0), "with the frequency");
        assert_ne!(plate_hue(k, 0.3, 0.1, 0.0), plate_hue(k, 0.3, 0.9, 0.0), "with the loudness");
        let m = CymaticsMotion { parts: vec![(14, 1.0)], target: 14, candidate: None, level: 0.8 };
        let lines = cymatics_body(&m, k, VizStyle::Dots, 120, 30, false, 3.0);
        let colours: std::collections::HashSet<_> = lines.iter().flat_map(|l| l.spans.iter().filter_map(|s| s.style.fg)).collect();
        assert!(colours.len() > 30, "{} colours", colours.len());
    }

    /// Louder music puts more sand on the plate.
    #[test]
    fn louder_is_more_sand() {
        let mut quiet = CymaticsMotion::default();
        let mut loud = CymaticsMotion::default();
        play(&mut quiet, 3, 0.32, 1.0);
        play(&mut loud, 3, 0.95, 1.0);
        let q = lit(&cymatics_body(&quiet, ThemeKind::Classic, VizStyle::Dots, 100, 30, false, 0.0));
        let l = lit(&cymatics_body(&loud, ThemeKind::Classic, VizStyle::Dots, 100, 30, false, 0.0));
        assert!(l > q * 3 / 2, "{l} vs {q}");
    }

    /// The motion follows audio time: the same audio in 50 ms or 10 ms steps
    /// lands on the same blend.
    #[test]
    fn the_morph_follows_audio_time_not_frame_count() {
        let mut bands = [0.4; SPECTRUM_BANDS];
        bands[17] = 0.9;
        let (mut a, mut b) = (CymaticsMotion::default(), CymaticsMotion::default());
        for _ in 0..6 {
            a.update(0.05, &bands);
        }
        for _ in 0..30 {
            b.update(0.01, &bands);
        }
        assert!((weight(&a, 17) - weight(&b, 17)).abs() < 0.03, "{} vs {}", weight(&a, 17), weight(&b, 17));
    }

    /// `Shift+L` adds the frequency and mode under the plate.
    #[test]
    fn the_readout_names_the_frequency_and_mode() {
        let mut m = CymaticsMotion::default();
        play(&mut m, 17, 0.8, 1.0); // 1 kHz
        let lines = cymatics_body(&m, ThemeKind::Classic, VizStyle::Dots, 80, 20, true, 0.0);
        let last = strip_ansi(lines.last().unwrap());
        let Mode { p, q, .. } = modes(plate_shape(80, 19).1)[17];
        assert!(last.contains("1 kHz") && last.contains(&format!("mode {p}·{q}")), "{last:?}");
        play(&mut m, 13, 0.8, 1.0); // 400 Hz
        let lines = cymatics_body(&m, ThemeKind::Classic, VizStyle::Dots, 80, 20, true, 0.0);
        assert!(strip_ansi(lines.last().unwrap()).contains("400 Hz"));
    }

    /// Time a frame: `cargo test --release viz_cymatics::tests::cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn cost() {
        let mut m = CymaticsMotion::default();
        play(&mut m, 9, 0.9, 0.1);
        play(&mut m, 20, 0.9, 0.1);
        play(&mut m, 28, 0.9, 0.1); // a three-way blend, the worst case seen
        for (w, rows) in [(100, 24), (200, 60)] {
            for style in [VizStyle::Bars, VizStyle::Dots] {
                let n = 50;
                let start = std::time::Instant::now();
                for i in 0..n {
                    std::hint::black_box(cymatics_body(&m, ThemeKind::HiFi, style, w, rows, true, i as f32 * 0.05));
                }
                let per = start.elapsed() / n;
                println!("{w}x{rows} {style:?} ({} patterns): {per:?} per frame, {:.1}% of a core at 20 fps", m.parts.len(), per.as_secs_f64() * 20.0 * 100.0);
            }
        }
    }

    /// Look at it: `cargo test viz_cymatics::tests::show -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn show() {
        for (band, w) in [(0, 60), (6, 60), (6, 120), (14, 120), (14, 170)] {
            let m = CymaticsMotion { parts: vec![(band, 1.0)], target: band, candidate: None, level: 0.7 };
            for style in [VizStyle::Dots] {
                println!("band {band} {style:?} {w} wide");
                for l in cymatics_body(&m, ThemeKind::Minimal, style, w, 14, true, 0.0) {
                    println!("|{}", strip_ansi(&l));
                }
            }
        }
    }
}
