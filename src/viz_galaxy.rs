//! The galaxy: a spiral of dust that moves with the music. Two arms broken
//! up by noise turn faster as the music gets louder; every bass hit (a kick)
//! flashes the galaxy and sends a shockwave out from its core; every
//! mid-range hit (a snare, a chord, a voice) sends a streak of colour, a new
//! one each time, out along one of the arms; every treble hit (a hi-hat, a
//! cymbal) bursts the stars round it; brighter sound shifts the colours
//! warmer. Silence or a pause leaves a dim galaxy turning slowly on its own.
//!
//! The music's part ([`GalaxyMotion`]) follows the AUDIO's time, like the VU
//! meter: hits are found per analysis hop from the raw band levels, split
//! as an equalizer splits them into bass, mids and treble
//! ([`GalaxyMotion::hop`]), loudness and colour advance by the audio
//! each frame covered ([`GalaxyMotion::update`]). Drawing
//! ([`render_galaxy`]) is a pure function of that motion and the wall-clock
//! time, so the idle turn keeps going while nothing plays.
//!
//! The look (a noisy spiral drawn with a `. : · + * o @` dust ramp) is
//! inspired by the splash screen of nebula
//! (<https://github.com/agentSystemLabs/nebula>).

use crate::state::{VizStyle, SPECTRUM_BANDS};
use crate::theme::{base_hue, hsv, Rgb, ThemeKind};
use crate::tui::LineBuf;
use crate::viz::{fit_width, Lines, BRAILLE_BITS};
use ratatui::text::Line;

/// The full-window ceiling. Past this the dust is only drawn bigger.
pub const GALAXY_MAX_ROWS: usize = 60;

/// A star at rest: centred in its cell, like the burst glyphs (`+`, `*`), so
/// a star does not jump up and down as a treble hit bursts it and it fades
/// (a `.` sits on the baseline). The dots style draws it as `⠆`, the two
/// middle dots of a braille cell, for the same reason.
const RESTING_STAR: char = '∙';

/// Glyph ramp for the bars style, thin dust to bright core.
const RAMP: [char; 8] = ['.', ':', '·', '+', '*', '*', 'o', '@'];

/// Below this the dust leaves a cell to the stars.
const FLOOR: f32 = 0.06;

/// The widest the disc is drawn, as columns per row: a terminal cell is
/// about twice as tall as wide, so 2 is round; past 3.6 a wide window would
/// flatten it into a streak.
const MAX_STRETCH: f32 = 3.6;

/// Radians a second the galaxy turns on its own, music or not.
const IDLE_SPIN: f32 = 0.06;

/// A sudden rise in one group of bands. Per hop the group's "rise" is the
/// spectral flux: each band's increase over the hop before (falls count as
/// nothing), averaged over the group — summing per band catches a hit
/// whose energy sits in a few bands, which the group's average level
/// smears. A rise is an onset when it stands out from the group's recent
/// rises: above their mean by ONSET_DEVS standard deviations, so a steady
/// busy bass line raises the bar and a sparse kick clears it; with a floor
/// so the noise of a quiet passage never counts.
///
/// And only while the group is really playing: loud enough to hear
/// (ONSET_QUIET) and within ONSET_BELOW_LOUDEST of the loudest it has been
/// lately. The bar alone adapts to anything — when the bass of a track
/// stopped (Le Flamand Noir, 6:08: −37 dB down to −80) the recent rises
/// shrank with it, and the leftover rumble's flicker kept the core bursting
/// for another fifteen seconds.
#[derive(Clone, Debug, Default)]
struct Onset {
    prev: Vec<f32>,
    /// The recent rises' mean and variance, over ONSET_MEMORY.
    mean: f32,
    var: f32,
    /// Seconds since the last onset.
    since: f32,
    /// The last hop's rise, so a hit can be told apart from its neighbours'
    /// (a kick lifts the mids a little too, a snare the bass).
    rise: f32,
    /// The loudest the group has been lately (its mean level), falling
    /// LOUDEST_FALL a second.
    loudest: f32,
}

/// How many standard deviations above the recent mean a rise must be.
const ONSET_DEVS: f32 = 2.0;
/// The least rise that counts: 0.04 of the 90 dB scale, about 3.5 dB.
const ONSET_FLOOR: f32 = 0.04;
/// How far back the running statistics look (seconds).
const ONSET_MEMORY: f32 = 1.5;
/// The quietest a group can be and still hit (its mean level, on the
/// bands' 90 dB scale): 0.25 is about −68 dB.
const ONSET_QUIET: f32 = 0.25;
/// How far under its loudest of late a group can be and still hit: 0.2 of
/// the scale is 18 dB.
const ONSET_BELOW_LOUDEST: f32 = 0.2;
/// How fast that loudest falls (scale a second: 0.012 is about 1 dB/s), so
/// a section that stays quiet becomes the new measure after a while.
const LOUDEST_FALL: f32 = 0.012;

impl Onset {
    /// One analysis hop of `secs` seconds with the group's bands at
    /// `levels`: true on an onset at least `gap` seconds after the last.
    fn hop(&mut self, levels: &[f32], secs: f32, gap: f32) -> bool {
        if self.prev.len() != levels.len() {
            self.prev = levels.to_vec();
            self.since += secs;
            return false;
        }
        let rise = levels.iter().zip(&self.prev).map(|(l, p)| (l - p).max(0.0)).sum::<f32>() / levels.len() as f32;
        self.prev.copy_from_slice(levels);
        let level = levels.iter().sum::<f32>() / levels.len() as f32;
        self.loudest = level.max(self.loudest - LOUDEST_FALL * secs);
        let playing = level >= ONSET_QUIET && level >= self.loudest - ONSET_BELOW_LOUDEST;
        self.since += secs;
        self.rise = rise;
        let threshold = ONSET_FLOOR.max(self.mean + ONSET_DEVS * self.var.sqrt());
        let k = 1.0 - (-secs / ONSET_MEMORY).exp();
        // A hit counts in the statistics only up to the bar it had to clear:
        // in a track with a kick on every beat the kicks would otherwise
        // raise the bar over themselves.
        let d = rise.min(threshold) - self.mean;
        self.mean += k * d;
        self.var = (1.0 - k) * (self.var + k * d * d);
        if playing && rise > threshold && self.since >= gap {
            self.since = 0.0;
            true
        } else {
            false
        }
    }
}

/// The three groups of the 31 bands, after the usual names for the
/// audible range (sub-bass 20–60 Hz and bass 60–250; low mids 250–500,
/// mids 500–2k and upper mids 2–4k; presence 4–6k and brilliance 6–20k),
/// a band going with the range its centre starts: bass 20–200 Hz (11
/// bands) sets off the beat — the core's flash and its shockwave; mids
/// 250 Hz–3.15 kHz (12) the colour streaks along the arms; treble
/// 4–20 kHz (8) the star bursts.
const BASS_BANDS: std::ops::Range<usize> = 0..11;
const MID_BANDS: std::ops::Range<usize> = 11..23;
const TREBLE_BANDS: std::ops::Range<usize> = 23..31;
/// A hit spills into the next group a little (a kick's click lifts the
/// mids, a snare's body the bass): a group's rise in a hop where its
/// neighbour rose this many times more is the neighbour's spill, not a hit.
const SPILL: f32 = 2.0;

/// The least time between two beats (seconds): 330 bpm.
const BEAT_GAP: f32 = 0.18;
/// The least time between two star bursts (seconds).
const BURST_GAP: f32 = 0.07;
/// The least time between two colour ripples (seconds).
const RIPPLE_GAP: f32 = 0.12;
/// Colour ripples travelling at once; a new one replaces the oldest.
const RIPPLES: usize = 4;

/// What the music has done to the galaxy so far.
#[derive(Clone, Debug)]
pub struct GalaxyMotion {
    /// How far the music has turned it (radians), on top of the idle spin.
    rot: f32,
    /// Loudness, smoothed (0..=1).
    energy: f32,
    /// Where the sound's weight sits across the bands, smoothed: 0 = all
    /// bass, 1 = all treble.
    centroid: f32,
    bass: Onset,
    mid: Onset,
    treble: Onset,
    /// Seconds of audio since the last beat and the last treble hit.
    beat_age: f32,
    burst_age: f32,
    /// Seconds of audio since each travelling colour ripple set off, and
    /// each one's number (which picks its colour and its arm).
    ripple_ages: [f32; RIPPLES],
    ripple_ids: [u32; RIPPLES],
    /// Ripples sent so far, numbering the next.
    ripples_sent: u32,
}

impl Default for GalaxyMotion {
    fn default() -> Self {
        GalaxyMotion {
            rot: 0.0,
            energy: 0.0,
            centroid: 0.0,
            bass: Onset::default(),
            mid: Onset::default(),
            treble: Onset::default(),
            // Long ago: nothing flashes before the first beat.
            beat_age: f32::INFINITY,
            burst_age: f32::INFINITY,
            ripple_ages: [f32::INFINITY; RIPPLES],
            ripple_ids: [0; RIPPLES],
            ripples_sent: 0,
        }
    }
}

impl GalaxyMotion {
    /// One analysis hop of `secs` seconds with `bands` its raw band levels
    /// (0..=1, before the spectrum's smoothing — which would blunt the very
    /// attacks a hit is): each group's hit sets off its effect, unless it is
    /// only the spill of a bigger rise next door (see [`SPILL`]).
    pub fn hop(&mut self, bands: &[f32; SPECTRUM_BANDS], secs: f32) {
        let bass = self.bass.hop(&bands[BASS_BANDS], secs, BEAT_GAP);
        let mid = self.mid.hop(&bands[MID_BANDS], secs, RIPPLE_GAP);
        let treble = self.treble.hop(&bands[TREBLE_BANDS], secs, BURST_GAP);
        let (b, m, t) = (self.bass.rise, self.mid.rise, self.treble.rise);
        if bass && b * SPILL > m {
            self.beat_age = 0.0;
        }
        if mid && m * SPILL > b && m * SPILL > t {
            // The oldest ripple makes way.
            let oldest = (0..RIPPLES).max_by(|&a, &b| self.ripple_ages[a].total_cmp(&self.ripple_ages[b])).unwrap_or(0);
            self.ripple_ages[oldest] = 0.0;
            self.ripples_sent = self.ripples_sent.wrapping_add(1);
            self.ripple_ids[oldest] = self.ripples_sent;
        }
        if treble && t * SPILL > m {
            self.burst_age = 0.0;
        }
    }

    /// Advance by `dt` seconds of audio at loudness `level` (the VU level,
    /// 0..=1) with `bands` the spectrum's (smoothed) band levels.
    pub fn update(&mut self, dt: f32, level: f32, bands: &[f32; SPECTRUM_BANDS]) {
        if dt <= 0.0 {
            return;
        }
        // Exponential followers: the share of the way to the target covered
        // in `dt` for time constant `tau`, the same at any frame rate.
        let follow = |tau: f32| 1.0 - (-dt / tau).exp();
        self.energy += (level.clamp(0.0, 1.0) - self.energy) * follow(0.3);
        self.rot += dt * 1.4 * self.energy;
        self.beat_age += dt;
        self.burst_age += dt;
        for age in &mut self.ripple_ages {
            *age += dt;
        }

        let total: f32 = bands.iter().sum();
        if total > 1e-3 {
            let weighted: f32 = bands.iter().enumerate().map(|(i, v)| i as f32 * v).sum();
            let c = weighted / total / (SPECTRUM_BANDS - 1) as f32;
            self.centroid += (c - self.centroid) * follow(0.5);
        }
    }
}

/// The flash a beat sets off fades with this time constant (seconds).
const FLASH_DECAY: f32 = 0.16;
/// The shockwave: how fast it travels (disc radii a second), how long it
/// lasts, and how thick it is (disc radii).
const WAVE_SPEED: f32 = 1.5;
const WAVE_LIFE: f32 = 0.9;
const WAVE_WIDTH: f32 = 0.07;
/// A star burst fades with this time constant (seconds).
const BURST_DECAY: f32 = 0.14;
/// A colour streak: how fast it travels out along its arm (disc radii a
/// second), how long it lasts, and how long a stretch of the arm it
/// colours (disc radii).
const RIPPLE_SPEED: f32 = 0.85;
const RIPPLE_LIFE: f32 = 1.6;
const RIPPLE_WIDTH: f32 = 0.13;

/// One colour streak under way, as a frame draws it.
#[derive(Clone, Copy, Default)]
struct Streak {
    /// How far out along its arm it is (disc radii), and its strength.
    at: f32,
    amp: f32,
    /// Its colour (hue, degrees) and the arm it runs along (0 or 1).
    hue: f32,
    arm: u8,
}

/// Which of the two spiral arms (0 or 1) the point (`dx`, `dy`) belongs
/// to, the arms turned by `rot`: the arm's phase (see [`density`]) counts
/// whole turns, and the two arms are the even and the odd ones.
fn arm_of(dx: f32, dy: f32, rot: f32) -> u8 {
    let r = (dx * dx + dy * dy).sqrt();
    let phase = 2.0 * dy.atan2(dx) - r * 5.5 + rot;
    ((phase / std::f32::consts::TAU).round() as i32).rem_euclid(2) as u8
}

fn hash(x: i32, y: i32, salt: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(374_761_393)
        ^ (y as u32).wrapping_mul(668_265_263)
        ^ salt.wrapping_mul(2_246_822_519);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^ (h >> 16)
}

/// Smooth 2D value noise, 0..=1.
fn vnoise(x: f32, y: f32) -> f32 {
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (x - x.floor(), y - y.floor());
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let at = |dx: i32, dy: i32| (hash(xi + dx, yi + dy, 991) & 0xffff) as f32 / 65535.0;
    let (a, b, c, d) = (at(0, 0), at(1, 0), at(0, 1), at(1, 1));
    a + (b - a) * sx + (c - a) * sy + (a - b - c + d) * sx * sy
}

/// Dust density at (`dx`, `dy`) from the core (in units of the disc's
/// radius): two spiral arms whose phase winds with the radius, broken up by
/// noise turning the other way, so wisps shear across the arms instead of
/// riding them; the core, swollen by a beat's flash; and the beat's
/// shockwave, a ring travelling outward.
fn density(dx: f32, dy: f32, sc: &Scene) -> f32 {
    let r = (dx * dx + dy * dy).sqrt().max(0.001);
    let theta = dy.atan2(dx);
    let arm = ((2.0 * theta - r * 5.5 + sc.rot).cos() * 0.5 + 0.5).powi(3);
    let (sa, ca) = (sc.rot * 0.3).sin_cos();
    let (nx, ny) = (dx * ca - dy * sa, dx * sa + dy * ca);
    let wisp = 0.5 + 0.5 * vnoise(nx * 6.0 + 7.0, ny * 6.0 + 3.0);
    let falloff = (-r * 2.6).exp();
    let core_r = 0.16 * (1.0 + 0.9 * sc.flash);
    let core = (-(r / core_r).powi(2)).exp() * (0.8 + 0.6 * sc.flash);
    let wave = sc.wave_amp * (-((r - sc.wave_r) / WAVE_WIDTH).powi(2)).exp() * wisp;
    (arm * wisp * falloff * sc.arms + core + wave).min(1.0)
}

/// What one frame is drawn from.
struct Scene {
    /// Wall-clock seconds, for the idle spin and the stars' twinkle.
    t: f32,
    rot: f32,
    arms: f32,
    /// The last beat's flash, 1 on the beat and fading.
    flash: f32,
    /// The shockwave's radius and strength (0 = none).
    wave_r: f32,
    wave_amp: f32,
    /// The last treble hit's burst, 1 on the hit and fading.
    burst: f32,
    /// The colour streaks under way.
    ripples: [Streak; RIPPLES],
    /// Overall brightness: dim in silence, full when loud, up on a beat.
    gain: f32,
    /// The hue the dust starts from, shifted warmer by brighter sound.
    hue: f32,
}

impl Scene {
    fn new(m: &GalaxyMotion, kind: ThemeKind, t: f32) -> Self {
        let flash = (-m.beat_age / FLASH_DECAY).exp();
        let (wave_r, wave_amp) = if m.beat_age < WAVE_LIFE {
            (0.1 + m.beat_age * WAVE_SPEED, 0.7 * (1.0 - m.beat_age / WAVE_LIFE).powi(2))
        } else {
            (0.0, 0.0)
        };
        Scene {
            t,
            rot: m.rot + t * IDLE_SPIN,
            arms: 1.5 + 1.3 * m.energy + 1.2 * flash,
            flash,
            wave_r,
            wave_amp,
            burst: (-m.burst_age / BURST_DECAY).exp(),
            ripples: std::array::from_fn(|i| {
                let age = m.ripple_ages[i];
                if age >= RIPPLE_LIFE {
                    return Streak::default();
                }
                // Its own colour, well away from the galaxy's, and its arm.
                let h = hash(m.ripple_ids[i] as i32, 77, 5_381);
                Streak {
                    at: 0.05 + age * RIPPLE_SPEED,
                    amp: (1.0 - age / RIPPLE_LIFE).powf(1.5),
                    hue: base_hue(kind) + 60.0 + (h % 240) as f32,
                    arm: ((h >> 12) & 1) as u8,
                }
            }),
            gain: (0.65 + 0.35 * m.energy.min(1.0).sqrt() + 0.3 * flash).min(1.0),
            // Brighter sound turns the hue toward red (down the wheel).
            hue: base_hue(kind) - 70.0 * (m.centroid - 0.35).max(0.0),
        }
    }

    /// The dust's colour at density `d`, (`dx`, `dy`) from the core:
    /// saturated and dark when faint, paler toward the bright core and in a
    /// beat's flash, the hue drifting a little outward; where a colour
    /// streak is passing along this arm, its own colour (and a little
    /// brighter).
    fn colour(&self, d: f32, dx: f32, dy: f32) -> Rgb {
        let r = (dx * dx + dy * dy).sqrt();
        let mut hue = self.hue + 35.0 * r.min(1.5);
        let mut crest = 0.0f32;
        if self.ripples.iter().any(|s| s.amp > 0.0) {
            let arm = arm_of(dx, dy, self.rot);
            for s in self.ripples.iter().filter(|s| s.amp > 0.0 && s.arm == arm) {
                let k = s.amp * (-((r - s.at) / RIPPLE_WIDTH).powi(2)).exp();
                if k > crest {
                    // Toward the streak's colour, the short way round.
                    let delta = (s.hue - hue + 540.0).rem_euclid(360.0) - 180.0;
                    hue = self.hue + 35.0 * r.min(1.5) + delta * k.min(1.0);
                    crest = k;
                }
            }
        }
        let crest = crest.min(1.0);
        let sat = (0.85 - 0.5 * d * d - 0.3 * self.flash + 0.15 * crest).clamp(0.15, 1.0);
        let value = ((0.3 + 0.7 * d) * self.gain.max(0.5) * (1.0 + 0.3 * crest)).min(1.0);
        hsv(hue, sat, value)
    }
}

/// Wall-clock seconds since the first galaxy frame of this run.
fn wall_time() -> f32 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f32()
}

/// See [`galaxy_body`]; this enforces the width half of the renderer
/// contract (`fit_width`).
pub fn render_galaxy(motion: &GalaxyMotion, kind: ThemeKind, style: VizStyle, width: usize, rows: usize) -> Lines {
    fit_width(galaxy_body(motion, kind, style, width, rows, wall_time()), width)
}

/// Exactly `rows` lines of the galaxy `t` seconds in, none wider than
/// `width`. Bars draws a glyph per cell from the dust ramp; Dots draws
/// braille, a dot per sub-cell, for finer dust.
fn galaxy_body(motion: &GalaxyMotion, kind: ThemeKind, style: VizStyle, width: usize, rows: usize, t: f32) -> Lines {
    if rows == 0 || width == 0 {
        return vec![Line::default(); rows];
    }
    let scene = Scene::new(motion, kind, t);
    // The disc fills the rows; across, it stops at MAX_STRETCH so a wide
    // window does not flatten it. Units: the disc's radius is 1.
    let span = (width as f32).min(rows as f32 * MAX_STRETCH);
    let (cx, cy) = (width as f32 / 2.0, rows as f32 / 2.0);
    let (sx, sy) = (2.0 / span, 2.0 / rows as f32);
    (0..rows)
        .map(|y| match style {
            VizStyle::Bars => glyph_row(&scene, y, width, (cx, cy, sx, sy)),
            VizStyle::Dots => braille_row(&scene, y, width, (cx, cy, sx, sy)),
        })
        .collect()
}

type Frame = (f32, f32, f32, f32);

fn glyph_row(scene: &Scene, y: usize, width: usize, (cx, cy, sx, sy): Frame) -> Line<'static> {
    let mut line = LineBuf::new();
    for x in 0..width {
        let (dx, dy) = ((x as f32 + 0.5 - cx) * sx, (y as f32 + 0.5 - cy) * sy);
        let d = density(dx, dy, scene) * scene.gain;
        let cell = if d >= FLOOR {
            let v = ((d - FLOOR) / (1.0 - FLOOR)).clamp(0.0, 1.0);
            let i = (v * (RAMP.len() - 1) as f32).round() as usize;
            Some((RAMP[i], scene.colour(v, dx, dy)))
        } else {
            star(scene, x, y)
        };
        match cell {
            Some((ch, c)) => {
                line.fg(c).push(ch);
            }
            None => {
                line.reset().push(' ');
            }
        }
    }
    trim_blanks(line)
}

/// A row with its trailing blanks dropped (they draw nothing).
fn trim_blanks(line: LineBuf) -> Line<'static> {
    let mut line = line.into_line();
    while line.spans.last().is_some_and(|s| s.content.trim().is_empty()) {
        line.spans.pop();
    }
    line
}

fn braille_row(scene: &Scene, y: usize, width: usize, (cx, cy, sx, sy): Frame) -> Line<'static> {
    let mut line = LineBuf::new();
    for x in 0..width {
        let mut bits = 0u32;
        let mut peak = 0.0f32;
        for (px, column) in BRAILLE_BITS.iter().enumerate() {
            for (py, bit) in column.iter().enumerate() {
                let fx = x as f32 + (px as f32 + 0.5) / 2.0;
                let fy = y as f32 + (py as f32 + 0.5) / 4.0;
                let (dx, dy) = ((fx - cx) * sx, (fy - cy) * sy);
                let d = density(dx, dy, scene) * scene.gain;
                // A dither fixed to the dot's place: faint dust lights a few
                // of a cell's dots, scattered (a regular pattern drew rows
                // of identical glyphs), dense dust all of them.
                let (gx, gy) = ((x * 2 + px) as i32, (y * 4 + py) as i32);
                let threshold = FLOOR + 0.55 * (hash(gx, gy, 4_243) & 0xff) as f32 / 255.0;
                if d >= threshold {
                    bits |= bit;
                    peak = peak.max(d);
                }
            }
        }
        if bits == 0 {
            match star(scene, x, y) {
                // A resting star is one braille dot here; a burst keeps its
                // bright glyph.
                Some((ch, c)) => line.fg(c).push(if ch == RESTING_STAR { '⠆' } else { ch }),
                None => line.reset().push(' '),
            };
            continue;
        }
        let (dx, dy) = ((x as f32 + 0.5 - cx) * sx, (y as f32 + 0.5 - cy) * sy);
        let c = scene.colour(peak, dx, dy);
        line.fg(c).push(char::from_u32(0x2800 + bits).unwrap_or(' '));
    }
    trim_blanks(line)
}

/// Star colours, hottest to coolest (O, B, A, F, G, K, M), and how many
/// stars in a hundred are of each: like a real sky, mostly the cool ones.
const STAR_CLASSES: [(Rgb, u32); 7] = [
    ((150, 175, 255), 3),
    ((175, 195, 255), 7),
    ((225, 230, 255), 10),
    ((255, 250, 235), 12),
    ((255, 232, 160), 18),
    ((255, 185, 115), 22),
    ((255, 135, 100), 28),
];

/// The colour of the star with hash `h`, at `brightness` (0..=1).
fn star_colour(h: u32, brightness: f32) -> Rgb {
    let mut pick = (h >> 16) % 100;
    let mut colour = STAR_CLASSES[STAR_CLASSES.len() - 1].0;
    for (c, share) in STAR_CLASSES {
        if pick < share {
            colour = c;
            break;
        }
        pick -= share;
    }
    let k = brightness.clamp(0.0, 1.0);
    ((colour.0 as f32 * k) as u8, (colour.1 as f32 * k) as u8, (colour.2 as f32 * k) as u8)
}

/// A star in empty sky: sparse, each its own colour (blue, white, yellow,
/// orange, red, as real ones are) and twinkling on its own phase — and on
/// a treble hit, a burst: many more come out, bright, then fade.
fn star(scene: &Scene, x: usize, y: usize) -> Option<(char, Rgb)> {
    let h = hash(x as i32, y as i32, 12_345);
    // A burst lights the stars that are always there and as many again
    // from cells that are dark otherwise, the strongest hit the most.
    let resting = h.is_multiple_of(67);
    let bursting = scene.burst > 0.08 && h % 29 == 3 && ((h >> 9) & 0xff) as f32 / 255.0 < scene.burst;
    if bursting {
        let glyph = if scene.burst > 0.6 { '*' } else { '+' };
        return Some((glyph, star_colour(h, 0.6 + 0.4 * scene.burst)));
    }
    if !resting {
        return None;
    }
    let phase = ((h >> 8) % 16) as f32 * 0.4;
    let tw = ((scene.t * 1.6 + phase).sin() * 0.5 + 0.5).max(scene.burst);
    if tw < 0.45 {
        return None;
    }
    Some((RESTING_STAR, star_colour(h, if tw > 0.85 { 0.85 } else { 0.5 })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::plain as strip_ansi;

    fn visible_len(l: &Line) -> usize {
        crate::tui::Cols::cols(l)
    }

    fn lit(lines: &[Line]) -> usize {
        lines.iter().map(|l| strip_ansi(l).chars().filter(|&c| !matches!(c, ' ' | RESTING_STAR | '⠆')).count()).sum()
    }

    /// Exactly the rows given, none wider than the window, at every size.
    #[test]
    fn every_size_fills_its_rows_and_fits_the_width() {
        let m = GalaxyMotion::default();
        for style in [VizStyle::Bars, VizStyle::Dots] {
            for w in (0..=40).chain([80, 133, 200]) {
                for rows in 0..=GALAXY_MAX_ROWS {
                    let lines = galaxy_body(&m, ThemeKind::HiFi, style, w, rows, 3.0);
                    assert_eq!(lines.len(), rows, "{style:?} {w}x{rows}");
                    for l in &lines {
                        assert!(visible_len(l) <= w, "{style:?} {w}x{rows}");
                    }
                }
            }
        }
    }

    /// One analysis hop, as the analyser runs them (2048 of 44.1k samples).
    const HOP: f32 = 2048.0 / 44_100.0;

    /// One hop of music `t` seconds in at `level`, with a kick every
    /// `kick` seconds and a hi-hat every `hat` seconds, fed as the analyser
    /// feeds it: the hop's raw bands, then the frame's update.
    fn step(m: &mut GalaxyMotion, t: f32, level: f32, kick: Option<f32>, hat: Option<f32>) {
        step_with(m, t, level, kick, None, hat);
    }

    /// `step` with a mid-range hit (a snare, say) every `snare` seconds too.
    /// Each hit lifts its own bands most and its neighbours' a little, as
    /// real ones do.
    fn step_with(m: &mut GalaxyMotion, t: f32, level: f32, kick: Option<f32>, snare: Option<f32>, hat: Option<f32>) {
        let hit = |every: Option<f32>| every.is_some_and(|e| t % e < HOP);
        let mut raw = [level * 0.6; SPECTRUM_BANDS];
        let mut lift = |r: std::ops::Range<usize>, by: f32| raw[r].iter_mut().for_each(|b| *b = (*b + by).min(1.0));
        if hit(kick) {
            lift(BASS_BANDS, 0.2);
            lift(MID_BANDS, 0.06);
        }
        if hit(snare) {
            lift(MID_BANDS, 0.15);
            lift(BASS_BANDS, 0.05);
        }
        if hit(hat) {
            lift(TREBLE_BANDS, 0.2);
        }
        m.hop(&raw, HOP);
        m.update(HOP, level, &[level * 0.6; SPECTRUM_BANDS]);
    }

    /// `secs` of that music.
    fn play(m: &mut GalaxyMotion, secs: f32, level: f32, kick: Option<f32>, hat: Option<f32>) {
        let mut t = 0.0;
        while t < secs {
            step(m, t, level, kick, hat);
            t += HOP;
        }
    }

    /// The beats and star bursts `secs` of that music sets off.
    fn count(m: &mut GalaxyMotion, secs: f32, kick: Option<f32>, hat: Option<f32>) -> (usize, usize) {
        let (mut beats, mut bursts) = (0, 0);
        let mut t = 0.0;
        while t < secs {
            step(m, t, 0.6, kick, hat);
            // A hit this hop leaves its age at exactly the hop's length.
            beats += usize::from(m.beat_age <= HOP);
            bursts += usize::from(m.burst_age <= HOP);
            t += HOP;
        }
        (beats, bursts)
    }

    /// A group that stops playing stops hitting: once the bass has dropped
    /// far under its level of a moment ago, the flicker of what is left sets
    /// nothing off, however small the recent rises have become; a quiet
    /// section that stays quiet becomes the measure again; and nothing under
    /// ONSET_QUIET ever hits.
    #[test]
    fn a_group_that_stops_playing_stops_hitting() {
        let mut o = Onset::default();
        let mut hits = 0;
        let mut t = 0.0f32;
        let mut run = |o: &mut Onset, secs: f32, base: f32, kick: f32| {
            let mut n = 0;
            let end = t + secs;
            while t < end {
                let on = (t % 0.5) < HOP;
                let level = if on { base + kick } else { base };
                n += usize::from(o.hop(&[level; 10], HOP, BEAT_GAP));
                t += HOP;
            }
            n
        };
        hits += run(&mut o, 10.0, 0.55, 0.15);
        assert!(hits >= 18, "the bass plays: {hits}");
        // The bass stops: −37 dB down to −80, with a flicker 4 dB high.
        assert_eq!(run(&mut o, 15.0, 0.12, 0.045), 0, "rumble far under the bass sets nothing off");
        // A quiet section that stays quiet becomes the measure: after a
        // while its hits count (above the absolute floor).
        let quiet_hits = run(&mut o, 30.0, 0.32, 0.1);
        assert!(quiet_hits >= 10, "a quiet section's own kicks count: {quiet_hits}");
        let mut fresh = Onset::default();
        assert_eq!(run(&mut fresh, 10.0, 0.1, 0.1), 0, "under the floor nothing hits");
    }

    /// Louder music turns it faster.
    #[test]
    fn loud_music_turns_it_faster() {
        let (mut quiet, mut loud) = (GalaxyMotion::default(), GalaxyMotion::default());
        play(&mut quiet, 5.0, 0.1, None, None);
        play(&mut loud, 5.0, 0.9, None, None);
        assert!(loud.rot > 3.0 * quiet.rot, "{} vs {}", loud.rot, quiet.rot);
    }

    /// Every kick is a beat, steady sound none; treble hits burst the stars
    /// and only they do.
    #[test]
    fn beats_follow_the_kicks_and_bursts_the_treble() {
        let mut m = GalaxyMotion::default();
        assert_eq!(count(&mut m, 4.0, None, None), (0, 0), "steady music: nothing fires");
        let (beats, bursts) = count(&mut m, 10.0, Some(0.5), None);
        assert!((19..=21).contains(&beats), "a kick every 0.5 s for 10 s: {beats} beats");
        assert_eq!(bursts, 0, "kicks do not burst the stars");
        let (beats, bursts) = count(&mut m, 10.0, None, Some(0.25));
        assert_eq!(beats, 0, "hi-hats are not beats");
        assert!((38..=41).contains(&bursts), "a hi-hat every 0.25 s for 10 s: {bursts} bursts");
    }

    /// Mid-range hits send colour ripples, not beats; a kick, which lifts
    /// the mids a little too, sends a beat and no ripple.
    #[test]
    fn mid_range_hits_ripple_the_colours_and_kicks_do_not() {
        let mut m = GalaxyMotion::default();
        let (mut beats, mut ripples) = (0, 0);
        let mut t = 0.0;
        while t < 10.0 {
            step_with(&mut m, t, 0.6, None, Some(0.5), None);
            beats += usize::from(m.beat_age <= HOP);
            ripples += usize::from(m.ripple_ages.iter().any(|&a| a <= HOP));
            t += HOP;
        }
        assert_eq!(beats, 0, "snares are not beats");
        assert!((19..=21).contains(&ripples), "a snare every 0.5 s for 10 s: {ripples} ripples");
        let (beats, _) = count(&mut m, 10.0, Some(0.5), None);
        assert!((19..=21).contains(&beats));
        assert!(m.ripple_ages.iter().all(|&a| a > 1.0), "kicks sent no ripples: {:?}", m.ripple_ages);
        // A streak colours its own arm where it is, not the other arm, not
        // far from it; it travels out; and each streak has its own colour.
        let mut m = GalaxyMotion::default();
        m.ripple_ages[0] = 0.3;
        m.ripple_ids[0] = 7;
        let sc = Scene::new(&m, ThemeKind::Classic, 0.0);
        let streak = sc.ripples[0];
        let calm = Scene::new(&GalaxyMotion::default(), ThemeKind::Classic, 0.0);
        // A point on each arm at the streak's radius.
        let on_arm = |arm: u8| -> (f32, f32) {
            (0..720).map(|i| i as f32 * std::f32::consts::TAU / 720.0)
                .map(|a| (streak.at * a.cos(), streak.at * a.sin()))
                .find(|&(x, y)| arm_of(x, y, sc.rot) == arm)
                .unwrap()
        };
        let (x, y) = on_arm(streak.arm);
        assert_ne!(sc.colour(0.5, x, y), calm.colour(0.5, x, y), "its arm is coloured");
        let (x, y) = on_arm(1 - streak.arm);
        assert_eq!(sc.colour(0.5, x, y), calm.colour(0.5, x, y), "the other arm is not");
        let far = (streak.at + 0.6, 0.0);
        assert_eq!(sc.colour(0.5, far.0, far.1), calm.colour(0.5, far.0, far.1), "far along, untouched");
        m.ripple_ages[0] = 1.0;
        assert!(Scene::new(&m, ThemeKind::Classic, 0.0).ripples[0].at > streak.at + 0.4, "it travels out");
        let hues: std::collections::HashSet<u32> = (1..20).map(|id| {
            m.ripple_ids[0] = id;
            Scene::new(&m, ThemeKind::Classic, 0.0).ripples[0].hue as u32
        }).collect();
        assert!(hues.len() > 10, "a new colour each time: {hues:?}");
    }

    /// A beat flashes the galaxy: just after one it is bigger and brighter
    /// than half a second later, and the shockwave has moved out.
    #[test]
    fn a_beat_flashes_and_sends_out_a_wave() {
        let mut m = GalaxyMotion::default();
        play(&mut m, 3.0, 0.5, None, None);
        m.beat_age = 0.02;
        let on_beat = galaxy_body(&m, ThemeKind::Minimal, VizStyle::Bars, 100, 24, 10.0);
        let wave_then = Scene::new(&m, ThemeKind::Minimal, 10.0).wave_r;
        m.update(0.5, 0.5, &[0.3; SPECTRUM_BANDS]);
        let later = galaxy_body(&m, ThemeKind::Minimal, VizStyle::Bars, 100, 24, 10.0);
        assert!(lit(&on_beat) > lit(&later) + 20, "{} vs {}", lit(&on_beat), lit(&later));
        assert!(Scene::new(&m, ThemeKind::Minimal, 10.0).wave_r > wave_then + 0.5, "the wave travels out");
    }

    /// A treble hit bursts the stars: far more are out just after one.
    #[test]
    fn a_treble_hit_bursts_the_stars() {
        let stars = |lines: &[Line]| -> usize {
            lines.iter().map(|l| strip_ansi(l).chars().filter(|c| matches!(c, '*' | '+')).count()).sum()
        };
        let mut m = GalaxyMotion::default();
        let calm = galaxy_body(&m, ThemeKind::HiFi, VizStyle::Dots, 120, 30, 4.0);
        m.burst_age = 0.01;
        let burst = galaxy_body(&m, ThemeKind::HiFi, VizStyle::Dots, 120, 30, 4.0);
        assert!(stars(&burst) > stars(&calm) + 30, "{} vs {}", stars(&burst), stars(&calm));
    }

    /// Time per call does not change where it ends up: 50 ms steps and
    /// 10 ms steps over the same audio turn it the same.
    #[test]
    fn motion_follows_audio_time_not_frame_count() {
        let bands = [0.5; SPECTRUM_BANDS];
        let (mut a, mut b) = (GalaxyMotion::default(), GalaxyMotion::default());
        for _ in 0..100 {
            a.update(0.05, 0.7, &bands);
        }
        for _ in 0..500 {
            b.update(0.01, 0.7, &bands);
        }
        assert!((a.rot - b.rot).abs() < 0.02 * a.rot, "{} vs {}", a.rot, b.rot);
        assert!((a.energy - b.energy).abs() < 0.01);
    }

    /// Silence still draws a (dimmer) galaxy, and it keeps turning with the
    /// clock alone.
    #[test]
    fn silence_leaves_a_dim_galaxy_turning() {
        let quiet = GalaxyMotion::default();
        let mut loud = GalaxyMotion::default();
        play(&mut loud, 3.0, 0.9, None, None);
        let q = galaxy_body(&quiet, ThemeKind::Minimal, VizStyle::Bars, 100, 24, 10.0);
        let l = galaxy_body(&loud, ThemeKind::Minimal, VizStyle::Bars, 100, 24, 10.0);
        assert!(lit(&q) > 30, "still a galaxy in silence: {}", lit(&q));
        assert!(lit(&l) > lit(&q), "louder is bigger and brighter: {} vs {}", lit(&l), lit(&q));
        let later = galaxy_body(&quiet, ThemeKind::Minimal, VizStyle::Bars, 100, 24, 14.0);
        assert_ne!(q, later, "the idle spin moves it");
    }

    /// Stars come in real star colours: many different ones, mostly warm.
    #[test]
    fn stars_have_real_star_colours() {
        let colours: Vec<Rgb> = (0..2000u32).map(|i| star_colour(hash(i as i32, 3, 9), 1.0)).collect();
        let distinct: std::collections::HashSet<Rgb> = colours.iter().copied().collect();
        assert_eq!(distinct.len(), STAR_CLASSES.len(), "every class shows up");
        let blue = colours.iter().filter(|c| c.2 > c.0).count();
        let red = colours.iter().filter(|c| c.0 as u32 > c.2 as u32 + 100).count();
        assert!(red > blue, "more cool red stars than hot blue ones: {red} vs {blue}");
    }

    /// Look at it: `cargo test viz_galaxy::tests::show -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn show() {
        let mut loud = GalaxyMotion::default();
        play(&mut loud, 4.0, 0.8, Some(0.5), None);
        let mut beat = loud.clone();
        beat.beat_age = 0.2;
        beat.burst_age = 0.02;
        for (what, m) in [("silent", GalaxyMotion::default()), ("loud", loud), ("beat + hi-hat", beat)] {
            for style in [VizStyle::Bars, VizStyle::Dots] {
                println!("{what} {style:?}");
                for l in galaxy_body(&m, ThemeKind::HiFi, style, 100, 14, 5.0) {
                    println!("|{}", strip_ansi(&l));
                }
            }
        }
    }

    /// Decode `path` (its first `secs` seconds) to interleaved samples.
    fn decode(path: &str, secs: f32) -> (Vec<f32>, usize, u32) {
        use symphonia::core::codecs::audio::AudioDecoderOptions;
        use symphonia::core::formats::probe::Hint;
        use symphonia::core::formats::{FormatOptions, TrackType};
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;
        let file = std::fs::File::open(path).expect("the file");
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let mut format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .unwrap();
        let track = format.default_track(TrackType::Audio).unwrap().clone();
        let params = track.codec_params.as_ref().and_then(|c| c.audio()).unwrap().clone();
        let rate = params.sample_rate.unwrap();
        let mut dec = symphonia::default::get_codecs().make_audio_decoder(&params, &AudioDecoderOptions::default()).unwrap();
        let (mut out, mut channels) = (Vec::new(), 2);
        while let Ok(Some(pkt)) = format.next_packet() {
            let Ok(buf) = dec.decode(&pkt) else { continue };
            channels = buf.spec().channels().count();
            let mut tmp = vec![0.0f32; buf.samples_interleaved()];
            buf.copy_to_slice_interleaved(&mut tmp);
            out.extend_from_slice(&tmp);
            if out.len() / channels > (secs * rate as f32) as usize {
                break;
            }
        }
        (out, channels, rate)
    }

    /// Mix kick drums into `samples` every `every` seconds from `from`: a
    /// sine falling 130 → 50 Hz with a fast attack and an 80 ms decay, at
    /// `peak` amplitude. Returns their start times.
    fn add_kicks(samples: &mut [f32], ch: usize, rate: u32, from: f32, every: f32, peak: f32) -> Vec<f32> {
        let frames = samples.len() / ch;
        let len = (0.25 * rate as f32) as usize;
        let mut times = Vec::new();
        let mut at = from;
        while ((at * rate as f32) as usize) < frames {
            let start = (at * rate as f32) as usize;
            let mut phase = 0.0f32;
            for i in 0..len.min(frames - start) {
                let t = i as f32 / rate as f32;
                let f = 50.0 + 80.0 * (-t / 0.03).exp();
                phase += 2.0 * std::f32::consts::PI * f / rate as f32;
                let env = (t / 0.002).min(1.0) * (-t / 0.08).exp();
                let v = peak * env * phase.sin();
                for c in 0..ch {
                    samples[(start + i) * ch + c] += v;
                }
            }
            times.push(at);
            at += every;
        }
        times
    }

    /// Precision and recall of `found` against `truth`, after shifting
    /// `found` by the lag (searched over 0..200 ms) that matches best, each
    /// truth used once, within ±`tol` seconds. Returns (precision, recall, lag).
    fn score(found: &[f32], truth: &[f32], tol: f32) -> (f32, f32, f32) {
        let matched = |lag: f32| {
            let mut used = vec![false; truth.len()];
            let mut hits = 0;
            for &f in found {
                let t = f - lag;
                if let Some(j) = (0..truth.len()).filter(|&j| !used[j] && (truth[j] - t).abs() <= tol).min_by(|&a, &b| (truth[a] - t).abs().total_cmp(&(truth[b] - t).abs())) {
                    used[j] = true;
                    hits += 1;
                }
            }
            hits
        };
        // The most hits at the SMALLEST lag: past the true lag, a lag up to `tol`
        // later scores the same, and the largest of those read as a delay.
        let (lag, hits) = (0..=40).map(|i| i as f32 * 0.005).map(|l| (l, matched(l))).fold((0.0, 0), |best, cur| if cur.1 > best.1 { cur } else { best });
        (hits as f32 / found.len().max(1) as f32, hits as f32 / truth.len().max(1) as f32, lag)
    }

    /// The live detector's beats on `samples`, fed through the real
    /// analyser in 50 ms chunks as the player feeds it, from `skip` seconds.
    fn live_beats(samples: &[f32], ch: usize, rate: u32, skip: f32) -> (Vec<f32>, usize, usize) {
        let state = crate::state::PlayerState::new();
        state.viz_mode.store(crate::state::VizMode::Galaxy as u8, std::sync::atomic::Ordering::Relaxed);
        let mut a = crate::viz::VizAnalyser::new(rate);
        let chunk = (rate as usize / 20) * ch;
        let (mut beats, mut ripples, mut bursts) = (Vec::new(), 0, 0);
        let mut t = 0.0f32;
        for piece in samples.chunks(chunk) {
            let (b0, r0, s0) = (a.galaxy.beat_age, a.galaxy.ripple_ages, a.galaxy.burst_age);
            a.process(piece, ch, &state);
            t += (piece.len() / ch) as f32 / rate as f32;
            if t < skip {
                continue;
            }
            let g = &a.galaxy;
            if g.beat_age < b0 { beats.push(t - g.beat_age); }
            if g.ripple_ages.iter().zip(r0).any(|(n, o)| *n < o) { ripples += 1; }
            if g.burst_age < s0 { bursts += 1; }
        }
        (beats, ripples, bursts)
    }

    /// A timeline of what the galaxy hears: `KEET_BEAT_FILE=<file>
    /// KEET_BEAT_SKIP=<from s> KEET_BEAT_SECS=<for s> cargo test --release
    /// viz_galaxy::tests::timeline -- --ignored --nocapture`. Every half
    /// second: the bass group's mean raw level (0..1 of the 90 dB scale, and
    /// in dB below full scale) and the beats fired in it.
    #[test]
    #[ignore]
    fn timeline() {
        let Ok(path) = std::env::var("KEET_BEAT_FILE") else { return };
        let env = |k: &str, d: f32| std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d);
        let (secs, skip) = (env("KEET_BEAT_SECS", 30.0), env("KEET_BEAT_SKIP", 0.0));
        let (samples, ch, rate) = decode(&path, skip + secs);
        let state = crate::state::PlayerState::new();
        state.viz_mode.store(crate::state::VizMode::Galaxy as u8, std::sync::atomic::Ordering::Relaxed);
        let mut a = crate::viz::VizAnalyser::new(rate);
        let chunk = (rate as usize / 20) * ch;
        let (mut t, mut next_print, mut beats, mut peak) = (0.0f32, skip, 0, 0.0f32);
        for piece in samples.chunks(chunk) {
            let b0 = a.galaxy.beat_age;
            a.process(piece, ch, &state);
            t += (piece.len() / ch) as f32 / rate as f32;
            if t < skip {
                continue;
            }
            beats += usize::from(a.galaxy.beat_age < b0);
            let bass = a.galaxy.bass.prev.iter().sum::<f32>() / a.galaxy.bass.prev.len().max(1) as f32;
            peak = peak.max(bass);
            if t >= next_print + 0.5 {
                println!("{:>3}:{:04.1}  bass {:.2} ({:>4.0} dB)  {}", (next_print / 60.0) as u32, next_print % 60.0, peak, peak * 90.0 - 90.0, "●".repeat(beats));
                next_print += 0.5;
                beats = 0;
                peak = 0.0;
            }
        }
    }

    /// The live detector on real music: `KEET_BEAT_FILE=<file> cargo test
    /// --release viz_galaxy::tests::real_music -- --ignored --nocapture`.
    /// It counts the beats fired on the track as it is (with no kick drum
    /// of its own, every one is false), then mixes kicks in at a known 120
    /// bpm and scores how many it catches: the two numbers to tune for.
    #[test]
    #[ignore]
    fn real_music() {
        let Ok(path) = std::env::var("KEET_BEAT_FILE") else { return };
        let env = |k: &str, d: f32| std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d);
        let (secs, skip, peak) = (env("KEET_BEAT_SECS", 40.0), env("KEET_BEAT_SKIP", 20.0), env("KEET_BEAT_PEAK", 0.35));
        let (mut samples, ch, rate) = decode(&path, skip + secs);
        let (plain, ripples, bursts) = live_beats(&samples, ch, rate, skip);
        let kicks = add_kicks(&mut samples, ch, rate, skip + 0.25, 0.5, peak);
        let (with, _, _) = live_beats(&samples, ch, rate, skip);
        let (p, r, lag) = score(&with, &kicks, 0.06);
        println!("{:.2}/s beats as is ({} ripples, {} bursts); added kicks: recall {:.0}%, precision {:.0}%, lag {:.0} ms",
            plain.len() as f32 / secs, ripples, bursts, r * 100.0, p * 100.0, lag * 1000.0);
    }

    /// Time a frame: `cargo test --release viz_galaxy::tests::cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn cost() {
        let mut m = GalaxyMotion::default();
        play(&mut m, 2.0, 0.8, Some(0.5), Some(0.25));
        m.beat_age = 0.2;
        m.burst_age = 0.02;
        for (w, rows) in [(100, 16), (200, 60)] {
            for style in [VizStyle::Bars, VizStyle::Dots] {
                let n = 50;
                let start = std::time::Instant::now();
                for i in 0..n {
                    std::hint::black_box(galaxy_body(&m, ThemeKind::HiFi, style, w, rows, i as f32 * 0.05));
                }
                let per = start.elapsed() / n;
                println!("{w}x{rows} {style:?}: {per:?} per frame, {:.1}% of a core at 20 fps", per.as_secs_f64() * 20.0 * 100.0);
            }
        }
    }
}
