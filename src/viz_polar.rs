//! The polar spectrum: the 31 bands round a circle, the left channel down
//! the left half and the right channel down the right, lows at the bottom
//! and treble at the top. Each band reaches out from a small hole in the
//! middle as far as its level, so a mix that leans to one side at some
//! frequencies shows as a lopsided shape there — what the bar spectrum,
//! which averages the channels, cannot show.
//!
//! In the dots style each band's outline leaves trails: the outlines of the
//! last half second, drawn inside the current one, each drifting further in
//! toward the centre and fading as it ages, so the shape seems to pour
//! inward as it moves. Both styles are painted from one palette running
//! from the centre outward — dark blue at the hole, through teal and green
//! and gold, to a pastel coral at full scale — so a loud band glows at its
//! tip. Trails take the palette's colour where they lie, darkened by their
//! age.
//!
//! Drawn in braille: a braille dot is half a cell wide and a quarter of one
//! tall, which on a cell about twice as tall as it is wide makes it square,
//! so a circle of dots stays round.

use crate::state::{PlayerState, VizStyle, SPECTRUM_BANDS};
use std::collections::VecDeque;
use crate::theme::{Rgb, ThemeKind};
use crate::tui::LineBuf;
use crate::viz::{fit_width, fmt_band_hz, viz_palette, Lines, VizPalette, BRAILLE_BITS, ISO_CENTERS};
use ratatui::style::Style;
use ratatui::text::Line;

/// The full-window ceiling: past this the circle only gets coarser bands.
pub const POLAR_MAX_ROWS: usize = 60;

/// The hole in the middle, as a share of the radius: a quiet band still
/// shows as a sliver instead of vanishing into the centre.
const INNER: f32 = 0.16;

/// Columns kept clear left and right of the circle for the band legend.
const LEGEND_W: usize = 5;

/// Bands named on the legend: 100 Hz, 1 kHz and 10 kHz.
const LEGEND_BANDS: [usize; 3] = [7, 17, 27];

/// The trails: one outline kept every TRAIL_STEP seconds of audio, TRAIL_LEN
/// of them (half a second), and how far toward the hole the oldest has
/// drifted (a share of its reach beyond the hole).
const TRAIL_STEP: f32 = 0.05;
const TRAIL_LEN: usize = 10;
const TRAIL_DRIFT: f32 = 0.75;

/// The outlines the trails are drawn from, newest first: each channel's
/// band levels as they were, one every TRAIL_STEP seconds of audio. Fed by
/// the analyser's per-frame step while the mode is on.
#[derive(Clone, Debug, Default)]
pub struct PolarTrail {
    outlines: VecDeque<([f32; SPECTRUM_BANDS], [f32; SPECTRUM_BANDS])>,
    since: f32,
}

impl PolarTrail {
    /// `dt` seconds of audio went by with the bands at `left` / `right`.
    pub fn push(&mut self, dt: f32, left: &[f32; SPECTRUM_BANDS], right: &[f32; SPECTRUM_BANDS]) {
        self.since += dt;
        while self.since >= TRAIL_STEP {
            self.since -= TRAIL_STEP;
            self.outlines.push_front((*left, *right));
            self.outlines.truncate(TRAIL_LEN);
        }
    }

    /// Forget them (the mode was left: coming back must not show old music).
    pub fn clear(&mut self) {
        self.outlines.clear();
        self.since = 0.0;
    }
}

/// See [`polar_body`]; this enforces the width half of the renderer contract
/// (`fit_width`).
pub fn render_polar(state: &PlayerState, trail: &PolarTrail, style: VizStyle, width: usize, rows: usize, extras: bool) -> Lines {
    let levels = Levels {
        left: state.get_spectrum(),
        right: state.get_spectrum_r(),
        peaks: state.get_dots(),
        trail: trail.outlines.iter().copied().collect(),
    };
    fit_width(polar_body(&levels, state.theme_kind(), style, width, rows, extras), width)
}

/// What the circle is drawn from: each channel's band levels (0..=1) and
/// the peak marks, which the analyser keeps for the two channels together.
struct Levels {
    left: [f32; SPECTRUM_BANDS],
    right: [f32; SPECTRUM_BANDS],
    peaks: [f32; SPECTRUM_BANDS],
    /// Earlier outlines, newest first (see [`PolarTrail`]).
    trail: Vec<([f32; SPECTRUM_BANDS], [f32; SPECTRUM_BANDS])>,
}

/// A dot's place in the circle: which channel's half, which band, and how
/// far out (in dots).
fn polar_of(dx: f32, dy: f32) -> (bool, usize, f32) {
    let rho = (dx * dx + dy * dy).sqrt();
    // Angle from straight down (0) up the side to straight up (π).
    let phi = dx.abs().atan2(dy);
    let band = ((phi / std::f32::consts::PI * SPECTRUM_BANDS as f32) as usize).min(SPECTRUM_BANDS - 1);
    (dx < 0.0, band, rho)
}

/// How far out (in dots) a level of `v` reaches on a circle of radius `r`.
fn reach(v: f32, r: f32) -> f32 {
    r * (INNER + (1.0 - INNER) * v.clamp(0.0, 1.0))
}

/// What one braille cell holds: its dots and the colour they take.
#[derive(Clone, Copy, Default)]
struct Cell {
    bits: u32,
    /// How far out its outermost level dot is, as a share of the band's
    /// travel (0 = the hole's edge, 1 = full scale), and that dot's band;
    /// None = no level dot.
    outer: Option<f32>,
    /// The youngest trail dot in it: its age (0 = newest, 1 = oldest) and
    /// how far out it lies (as `outer`).
    echo: Option<(f32, f32)>,
    /// How far out its peak-hold dots lie (as `outer`).
    peak_share: f32,
}

fn polar_body(levels: &Levels, kind: ThemeKind, style: VizStyle, width: usize, rows: usize, extras: bool) -> Lines {
    let vp = viz_palette(kind);
    // The circle's box is square on screen: `side` rows by twice as many
    // columns. Whichever of height or width runs out first sets it.
    let legend = extras && width >= 2 * (LEGEND_W + 4);
    let room = width.saturating_sub(if legend { 2 * LEGEND_W } else { 0 } + 2);
    let side = rows.min(room / 2);
    if side < 2 {
        return vec![Line::default(); rows];
    }
    let cols = side * 2;
    let (dots_w, dots_h) = (cols * 2, side * 4);
    let (cx, cy) = (dots_w as f32 / 2.0, dots_h as f32 / 2.0);
    let r = dots_h as f32 / 2.0 - 0.5;

    let mut cells = vec![Cell::default(); cols * side];
    let mut peak_only = vec![false; cols * side];
    for gy in 0..dots_h {
        for gx in 0..dots_w {
            let (dx, dy) = (gx as f32 + 0.5 - cx, gy as f32 + 0.5 - cy);
            let (left, band, rho) = polar_of(dx, dy);
            if rho > r + 0.5 {
                continue;
            }
            let level = if left { levels.left[band] } else { levels.right[band] };
            let edge = reach(level, r);
            let lit = match style {
                VizStyle::Bars => rho >= r * INNER && rho <= edge,
                // The outline alone: where each band's level ends.
                VizStyle::Dots => rho >= r * INNER && (rho - edge).abs() <= 1.0,
            };
            let peak = (rho - reach(levels.peaks[band], r)).abs() <= 0.6 && rho > edge + 1.0;
            // A trail dot: an earlier outline, inside the current one.
            let echo = match style {
                VizStyle::Dots if !lit && rho >= r * INNER && rho < edge - 1.0 => trail_age(levels, left, band, rho, r),
                _ => None,
            };
            if !lit && !peak && echo.is_none() {
                continue;
            }
            let (x, y) = (gx / 2, gy / 4);
            let cell = &mut cells[y * cols + x];
            cell.bits |= BRAILLE_BITS[gx % 2][gy % 4];
            let share = ((rho / r - INNER) / (1.0 - INNER)).clamp(0.0, 1.0);
            if lit {
                if cell.outer.is_none_or(|o| share > o) {
                    cell.outer = Some(share);
                }
            } else if let Some(age) = echo {
                if cell.echo.is_none_or(|(a, _)| age < a) {
                    cell.echo = Some((age, share));
                }
            } else if cell.outer.is_none() {
                peak_only[y * cols + x] = true;
                cell.peak_share = share;
            }
        }
    }

    // The circle centred in the window; each line built as (glyph, colour)
    // per column so the legend can go into the blank cells around it.
    let x0 = (width - cols) / 2;
    let mut grid: Vec<Vec<(char, Style)>> = vec![vec![(' ', Style::new()); width]; side];
    for y in 0..side {
        for x in 0..cols {
            let cell = cells[y * cols + x];
            if cell.bits == 0 {
                continue;
            }
            let peak = peak_only[y * cols + x];
            let colour = crate::tui::fg(cell_colour(&cell, peak));
            grid[y][x0 + x] = (char::from_u32(0x2800 + cell.bits).unwrap_or(' '), colour);
        }
    }
    if legend {
        put_legend(&mut grid, x0, cols, side, &vp);
    }

    let mut lines: Lines = grid.iter().map(|row| join_row(row)).collect();
    // The box is square, the budget may not be: centre it in the rows given.
    let top = (rows - side) / 2;
    let mut out = vec![Line::default(); top];
    out.append(&mut lines);
    out.resize(rows, Line::default());
    out
}

/// Whether a dot `rho` out in `band` of one half lies on a trail, and if
/// so the youngest one's age (0..1). Trail `k` is the outline from k + 1
/// steps ago, pulled toward the hole by its age so the trails drift inward.
fn trail_age(levels: &Levels, left: bool, band: usize, rho: f32, r: f32) -> Option<f32> {
    let hole = r * INNER;
    levels.trail.iter().enumerate().find_map(|(k, (l, rt))| {
        let age = (k + 1) as f32 / TRAIL_LEN as f32;
        let level = if left { l[band] } else { rt[band] };
        let at = hole + (reach(level, r) - hole) * (1.0 - TRAIL_DRIFT * age);
        ((rho - at).abs() <= 0.5).then_some(age)
    })
}

/// The palette's stops, from the hole's edge (0) to full scale (1).
const PALETTE: [(f32, Rgb); 6] = [
    (0.00, (18, 28, 96)),    // dark blue
    (0.25, (20, 92, 168)),   // blue
    (0.45, (16, 150, 150)),  // teal
    (0.65, (70, 195, 95)),   // green
    (0.82, (225, 210, 95)),  // gold
    (1.00, (255, 168, 140)), // pastel coral
];

/// The palette at `share` of the way out (0 = the hole's edge, 1 = full
/// scale), blended between its stops.
fn radial(share: f32) -> Rgb {
    let s = share.clamp(0.0, 1.0);
    let i = PALETTE.iter().rposition(|&(at, _)| at <= s).unwrap_or(0).min(PALETTE.len() - 2);
    let ((a, ca), (b, cb)) = (PALETTE[i], PALETTE[i + 1]);
    let f = ((s - a) / (b - a)).clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    (mix(ca.0, cb.0), mix(ca.1, cb.1), mix(ca.2, cb.2))
}

/// A cell's colour: the palette where its outermost level dot lies; a trail
/// the palette where it lies, darker the older it is; the peak ring a dim
/// tint of the palette's edge.
fn cell_colour(cell: &Cell, peak: bool) -> Rgb {
    let dim = |(r, g, b): Rgb, k: f32| ((r as f32 * k) as u8, (g as f32 * k) as u8, (b as f32 * k) as u8);
    match (cell.outer, cell.echo) {
        (Some(share), _) => radial(share),
        (None, Some((age, share))) => dim(radial(share), 1.0 - 0.7 * age),
        _ if peak => dim(radial(cell.peak_share), 0.5),
        _ => radial(0.5),
    }
}

/// The legend, in the blank cells round the circle: `L` and `R` under each
/// half, and 100 Hz / 1 kHz / 10 kHz beside the circle at their bands' height
/// on both sides.
fn put_legend(grid: &mut [Vec<(char, Style)>], x0: usize, cols: usize, side: usize, vp: &VizPalette) {
    let width = grid.first().map_or(0, Vec::len);
    let write = |grid: &mut [Vec<(char, Style)>], y: usize, x: usize, text: &str| {
        let row = &mut grid[y];
        let n = text.chars().count();
        if x + n > width || row[x..x + n].iter().any(|(c, _)| *c != ' ') {
            return;
        }
        for (i, ch) in text.chars().enumerate() {
            row[x + i] = (ch, vp.dim);
        }
    };
    let bottom = side - 1;
    write(grid, bottom, x0 + cols / 4, "L");
    write(grid, bottom, x0 + cols * 3 / 4, "R");
    for band in LEGEND_BANDS {
        // The band's middle angle, from straight down (row side - 1) to
        // straight up (row 0).
        let phi = (band as f32 + 0.5) / SPECTRUM_BANDS as f32 * std::f32::consts::PI;
        let y = ((1.0 + phi.cos()) / 2.0 * (side - 1) as f32).round() as usize;
        // Beside the full-scale circle on that row, so the label stays put
        // as the levels move: the row's widest dot row sets its half-width.
        let (cy, r) = (side as f32 * 2.0, side as f32 * 2.0 - 0.5);
        let dy = (0..4).map(|py| ((y * 4 + py) as f32 + 0.5 - cy).abs()).fold(f32::MAX, f32::min);
        let half = ((r * r - dy * dy).max(0.0).sqrt() / 2.0).ceil() as usize;
        let (first, last) = (x0 + cols / 2 - half.min(cols / 2), x0 + cols / 2 + half.min(cols / 2));
        let label = fmt_band_hz(ISO_CENTERS[band]);
        if let Some(x) = first.checked_sub(2 + label.chars().count()) {
            write(grid, y, x, &label);
        }
        write(grid, y, last + 2, &label);
    }
}

/// One row of (glyph, colour) cells as a line, trailing blanks dropped.
fn join_row(row: &[(char, Style)]) -> Line<'static> {
    let end = row.iter().rposition(|(c, _)| *c != ' ').map_or(0, |i| i + 1);
    let mut line = LineBuf::new();
    for &(ch, colour) in &row[..end] {
        if ch == ' ' {
            line.reset();
        } else {
            line.style(colour);
        }
        line.push(ch);
    }
    line.into_line()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::plain as strip_ansi;

    fn visible_len(l: &Line) -> usize {
        crate::tui::Cols::cols(l)
    }

    fn levels(left: f32, right: f32) -> Levels {
        Levels { left: [left; SPECTRUM_BANDS], right: [right; SPECTRUM_BANDS], peaks: [0.0; SPECTRUM_BANDS], trail: Vec::new() }
    }

    /// Lit braille dots in a rendered block, left half and right half.
    /// The circle is centred in the window, so its halves split at `width / 2`.
    fn dots_by_half(lines: &[Line], width: usize) -> (u32, u32) {
        let plain: Vec<Vec<char>> = lines.iter().map(|l| strip_ansi(l).chars().collect()).collect();
        let (mut l, mut r) = (0, 0);
        for row in &plain {
            for (x, &c) in row.iter().enumerate() {
                let bits = (c as u32).wrapping_sub(0x2800);
                if bits > 0 && bits <= 0xFF {
                    if x < width / 2 { l += bits.count_ones() } else { r += bits.count_ones() }
                }
            }
        }
        (l, r)
    }

    /// Exactly the rows given, none wider than the window, at every size.
    #[test]
    fn every_size_fills_its_rows_and_fits_the_width() {
        let lv = levels(0.7, 0.4);
        for style in [VizStyle::Bars, VizStyle::Dots] {
            for extras in [false, true] {
                for w in 1..=120 {
                    for rows in 0..=POLAR_MAX_ROWS {
                        let lines = polar_body(&lv, ThemeKind::Minimal, style, w, rows, extras);
                        assert_eq!(lines.len(), rows, "{style:?} {extras} {w}x{rows}");
                        for l in &lines {
                            assert!(visible_len(l) <= w, "{style:?} {extras} {w}x{rows}: {l:?}");
                        }
                    }
                }
            }
        }
    }

    /// The left channel draws on the left, the right on the right: a louder
    /// left channel lights more of the left half.
    #[test]
    fn each_channel_has_its_own_half() {
        let (l, r) = dots_by_half(&polar_body(&levels(0.9, 0.2), ThemeKind::Classic, VizStyle::Bars, 80, 20, false), 80);
        assert!(l > 2 * r, "left {l} right {r}");
        let (l, r) = dots_by_half(&polar_body(&levels(0.2, 0.9), ThemeKind::Classic, VizStyle::Bars, 80, 20, false), 80);
        assert!(r > 2 * l, "left {l} right {r}");
        let (l, r) = dots_by_half(&polar_body(&levels(0.6, 0.6), ThemeKind::Classic, VizStyle::Bars, 80, 20, false), 80);
        assert!(l.abs_diff(r) * 10 <= l + r, "a centred mix is symmetric: {l} vs {r}");
    }

    /// Lows sit at the bottom, treble at the top: a bass-only signal lights
    /// the lower half of the circle, a treble-only one the upper.
    #[test]
    fn lows_at_the_bottom_treble_at_the_top() {
        let mut bass = levels(0.0, 0.0);
        let mut treble = levels(0.0, 0.0);
        for i in 0..8 {
            bass.left[i] = 1.0;
            bass.right[i] = 1.0;
            treble.left[SPECTRUM_BANDS - 1 - i] = 1.0;
            treble.right[SPECTRUM_BANDS - 1 - i] = 1.0;
        }
        let lit_rows = |lv: &Levels| -> (u32, u32) {
            let lines = polar_body(lv, ThemeKind::Classic, VizStyle::Bars, 80, 20, false);
            let (top, bottom) = lines.split_at(lines.len() / 2);
            let count = |ls: &[Line]| ls.iter().map(|l| strip_ansi(l).chars().map(|c| (c as u32).wrapping_sub(0x2800).min(0x100).count_ones()).sum::<u32>()).sum::<u32>();
            (count(top), count(bottom))
        };
        let (top, bottom) = lit_rows(&bass);
        assert!(bottom > 3 * top, "bass: top {top} bottom {bottom}");
        let (top, bottom) = lit_rows(&treble);
        assert!(top > 3 * bottom, "treble: top {top} bottom {bottom}");
    }

    /// The circle is round: as many dots across as down.
    #[test]
    fn the_circle_is_round() {
        let lines = polar_body(&levels(1.0, 1.0), ThemeKind::Classic, VizStyle::Bars, 120, 20, false);
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        let lit_cols = |row: &str| row.chars().filter(|&c| c != ' ').count();
        let widest = plain.iter().map(|r| lit_cols(r)).max().unwrap();
        let tall = plain.iter().filter(|r| lit_cols(r) > 0).count();
        // Cells are twice as tall as wide: a round circle is 2x as many
        // columns as rows.
        assert!((widest as f32 / tall as f32 - 2.0).abs() < 0.25, "{widest} cols x {tall} rows");
    }

    /// The legend names L, R and three frequencies, and never writes over
    /// the circle.
    #[test]
    fn the_legend_labels_the_halves_and_frequencies() {
        let lv = levels(1.0, 1.0);
        let with = polar_body(&lv, ThemeKind::Classic, VizStyle::Bars, 100, 24, true);
        let text = with.iter().map(|l| strip_ansi(l)).collect::<Vec<_>>().join("\n");
        for label in ["L", "R", "100", "1k", "10k"] {
            assert!(text.contains(label), "{label} missing:\n{text}");
        }
        let without = polar_body(&lv, ThemeKind::Classic, VizStyle::Bars, 100, 24, false);
        let (wl, wr) = dots_by_half(&with, 100);
        let (nl, nr) = dots_by_half(&without, 100);
        assert!(wl <= nl && wr <= nr && wl > 0, "the legend makes room, it does not draw over dots");
    }

    /// In the dots style the trails fill the inside of the outline, newest
    /// outermost; the bars style (already filled) draws none.
    #[test]
    fn trails_fill_the_inside_in_dots() {
        let mut trail = PolarTrail::default();
        for k in 0..TRAIL_LEN {
            let v = 0.9 - 0.04 * k as f32;
            trail.push(TRAIL_STEP, &[v; SPECTRUM_BANDS], &[v; SPECTRUM_BANDS]);
        }
        let mut with = levels(0.5, 0.5);
        with.trail = trail.outlines.iter().copied().collect();
        let without = levels(0.5, 0.5);
        let lit = |lv: &Levels, style| {
            let (l, r) = dots_by_half(&polar_body(lv, ThemeKind::Classic, style, 100, 24, false), 100);
            l + r
        };
        assert!(lit(&with, VizStyle::Dots) > 2 * lit(&without, VizStyle::Dots), "{} vs {}", lit(&with, VizStyle::Dots), lit(&without, VizStyle::Dots));
        assert_eq!(lit(&with, VizStyle::Bars), lit(&without, VizStyle::Bars));
        // Half a second of outlines, the oldest dropped.
        trail.push(TRAIL_STEP * 3.0, &[0.1; SPECTRUM_BANDS], &[0.1; SPECTRUM_BANDS]);
        assert_eq!(trail.outlines.len(), TRAIL_LEN);
        assert_eq!(trail.outlines[0].0[0], 0.1, "newest first");
    }

    /// Both styles are painted from one palette, dark at the centre and
    /// light at the edge; a trail darker than the outline, older darker still.
    #[test]
    fn the_palette_runs_dark_inside_to_light_outside() {
        let kind = ThemeKind::Minimal;
        let lum = |c: Rgb| c.0 as u32 + c.1 as u32 + c.2 as u32;
        let outline = |share| Cell { bits: 1, outer: Some(share), ..Cell::default() };
        let trail = |age, share| Cell { bits: 1, echo: Some((age, share)), ..Cell::default() };
        assert!(lum(cell_colour(&outline(0.1), false)) < lum(cell_colour(&outline(0.5), false)));
        assert!(lum(cell_colour(&outline(0.5), false)) < lum(cell_colour(&outline(0.95), false)));
        assert!(lum(cell_colour(&trail(0.2, 0.5), false)) < lum(cell_colour(&outline(0.5), false)));
        assert!(lum(cell_colour(&trail(0.9, 0.5), false)) < lum(cell_colour(&trail(0.2, 0.5), false)));
        // Dark blue at the hole, warm at the tips.
        let (inner, outer) = (radial(0.0), radial(1.0));
        assert!(inner.2 > inner.0 && outer.0 > outer.2, "{inner:?} … {outer:?}");
        // A drawn frame in either style has many colours, not the theme's two.
        let mut lv = levels(0.8, 0.8);
        lv.trail = (0..TRAIL_LEN).map(|k| ([0.6 + 0.03 * k as f32; SPECTRUM_BANDS], [0.6 + 0.03 * k as f32; SPECTRUM_BANDS])).collect();
        for style in [VizStyle::Dots, VizStyle::Bars] {
            let lines = polar_body(&lv, kind, style, 100, 24, false);
            let colours: std::collections::HashSet<_> = lines.iter().flat_map(|l| l.spans.iter().filter_map(|s| s.style.fg)).collect();
            assert!(colours.len() > 15, "{style:?}: {} colours", colours.len());
        }
    }

    /// Look at it: `cargo test viz_polar::tests::show -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn show() {
        // A mix leaning left in the lows and right in the highs.
        let lv = Levels {
            left: std::array::from_fn(|i| 0.85 - i as f32 * 0.02),
            right: std::array::from_fn(|i| 0.35 + i as f32 * 0.015),
            peaks: std::array::from_fn(|i| 0.92 - i as f32 * 0.005),
            // Falling from louder over the last half second.
            trail: (0..TRAIL_LEN)
                .map(|k| {
                    let up = 0.02 * (k + 1) as f32;
                    (std::array::from_fn(|i| 0.85 - i as f32 * 0.02 + up), std::array::from_fn(|i| 0.35 + i as f32 * 0.015 + up))
                })
                .collect(),
        };
        for style in [VizStyle::Bars, VizStyle::Dots] {
            for l in polar_body(&lv, ThemeKind::Classic, style, 90, 18, true) {
                println!("|{}", strip_ansi(&l));
            }
            println!();
        }
    }
}
