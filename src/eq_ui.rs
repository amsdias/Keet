//! EQ editor screen — one shared, palette-driven renderer for the 10-band
//! parametric EQ, used by all three themes (like the library tree body). Pure:
//! it takes the band set and the selection and returns the lines (the dialog
//! of every theme, and HiFi's EQ unit through `eq_body`).

use crate::eq::{format_freq, BandSettings, BandType, EQ_BANDS, EQ_GAIN_LIMIT};
use crate::theme::Palette;
use crate::tui::LineBuf;
use ratatui::style::Style;
use ratatui::text::Line;

/// Append `s` (one glyph wide, `vis` columns) to `line` centred in `w`
/// columns, in `style`.
fn center_cell(line: &mut LineBuf, s: &str, style: Style, vis: usize, w: usize) {
    let pad = w.saturating_sub(vis);
    let lp = pad / 2;
    line.reset().spaces(lp).styled(s, style).reset().spaces(pad - lp);
}

/// "105 Hz" / "3.2 kHz" — a frequency in running text (the column labels
/// use the unitless `format_freq`).
fn freq_with_unit(freq: f32) -> String {
    if freq >= 1000.0 {
        format!("{:.1} kHz", freq / 1000.0)
    } else {
        format!("{:.0} Hz", freq)
    }
}

/// Long-form band description for the selected-band readout, e.g.
/// `BAND 3 · LOW SHELF · 105 Hz · +5.5 dB · Q 0.71` (gain omitted for cuts).
pub fn band_readout(selected: usize, b: &BandSettings, p: &Palette) -> Line<'static> {
    let kind = match b.kind {
        BandType::Peak => "PEAK",
        BandType::LowShelf => "LOW SHELF",
        BandType::HighShelf => "HIGH SHELF",
        BandType::LowCut => "LOW CUT",
        BandType::HighCut => "HIGH CUT",
    };
    let mut line = LineBuf::new();
    let sep = |line: &mut LineBuf| {
        line.reset().push(' ').styled("·", p.rule).reset().push(' ');
    };
    line.push_str("  ").styled("BAND", p.dim).reset().push(' ').styled(&(selected + 1).to_string(), p.accent);
    sep(&mut line);
    line.styled(kind, p.fg);
    sep(&mut line);
    line.styled(&freq_with_unit(b.freq), p.fg);
    if b.kind.uses_gain() {
        sep(&mut line);
        line.styled(&format!("{:+.1} dB", b.gain), p.fg);
    }
    sep(&mut line);
    line.styled(&format!("Q {:.2}", b.q), p.fg);
    line.into_line()
}

/// The headroom row: the curve's highest boost against the preamp, whether
/// loud tracks will reach the limiter, and the key that fixes it.
/// `with_key`: offer the `a` key (only where it works: the EQ has the keys).
pub fn headroom_line(h: &crate::eq::Headroom, preamp_db: f32, with_key: bool, p: &Palette) -> Line<'static> {
    let mut line = LineBuf::new();
    let sep = |line: &mut LineBuf| {
        line.reset().push_str("  ").styled("·", p.rule).reset().push_str("  ");
    };
    line.push_str("  ").styled("HEADROOM", p.dim).reset().push_str("  ");
    if h.max_boost_db < 0.05 {
        line.styled("no boost", p.fg);
    } else {
        line.styled(&format!("max boost {:+.1} dB", h.max_boost_db), p.fg)
            .reset()
            .push(' ')
            .styled(&format!("@ {}", freq_with_unit(h.at_hz)), p.dim);
    }
    sep(&mut line);
    line.styled(&format!("preamp {:+.1} dB", preamp_db), p.fg);
    if h.over_db >= 0.05 {
        sep(&mut line);
        line.styled(&format!("▲ {:.1} dB over", h.over_db), p.warn);
    } else if h.max_boost_db >= 0.05 {
        sep(&mut line);
        line.styled("✓ covered", p.good);
    }
    if let Some(s) = h.suggested_preamp.filter(|_| with_key) {
        sep(&mut line);
        line.styled("[a]", p.accent).reset().push(' ').styled(&format!("set preamp {:+.1} dB", s), p.dim);
    }
    line.into_line()
}

/// Slider rows for a budget of `rows`: odd, so there is a true centre
/// (0 dB) row — rounded DOWN (`| 1` rounded an even budget up, one row past
/// what the caller had) — and 5 to 15.
pub fn slider_rows(rows: usize) -> usize {
    let h = rows.clamp(5, 15);
    if h.is_multiple_of(2) { h - 1 } else { h }
}

/// Columns of the gain scale left of the sliders ("  +12 ").
pub const SCALE_W: usize = 6;

/// The sliders and the three label rows under them (frequency, filter type,
/// gain): `slider_h + 3` lines, `SCALE_W + EQ_BANDS * band_w` columns. A
/// knob at each band's gain on a 0 dB baseline; `selected` is lit (None:
/// none, the faceplate without the keys). Cut bands (no gain axis) park
/// their knob on the baseline in the warn color; the type row tells them
/// apart. `knob` is the theme's glyph (● / ◆ / █). `faders`: how a band
/// is drawn (`Faders`).
#[allow(clippy::too_many_arguments)]
pub fn eq_body(
    bands: &[BandSettings; EQ_BANDS],
    selected: Option<usize>,
    knob: char,
    faders: Faders,
    p: &Palette,
    band_w: usize,
    slider_h: usize,
) -> Vec<Line<'static>> {
    let center = slider_h / 2;
    // gain (dB) → grid row (0 = top = +limit … slider_h-1 = bottom = -limit).
    let row_of = |g: f32| -> usize {
        let t = (EQ_GAIN_LIMIT - g) / (2.0 * EQ_GAIN_LIMIT);
        (t * (slider_h - 1) as f32).round().clamp(0.0, (slider_h - 1) as f32) as usize
    };
    let mut out = Vec::with_capacity(slider_h + 3);
    let rail = faders != Faders::Lines;
    // Dots: four a row, the last unused so 0 dB is a dot row of its own.
    let travel = (4 * slider_h).saturating_sub(2);
    let dot_of = |g: f32| -> usize {
        let t = (EQ_GAIN_LIMIT - g) / (2.0 * EQ_GAIN_LIMIT);
        (t * travel as f32).round().clamp(0.0, travel as f32) as usize
    };

    // Slider grid.
    let knob = knob.to_string();
    let rail_style = if rail { p.dim } else { p.rule };
    for r in 0..slider_h {
        let major = r == 0 || r == center || r == slider_h - 1;
        let scale = if r == 0 {
            format!("+{}", EQ_GAIN_LIMIT as i32)
        } else if r == center {
            "0".to_string()
        } else if r == slider_h - 1 {
            format!("{}", -(EQ_GAIN_LIMIT as i32))
        } else {
            String::new()
        };
        let mut line = LineBuf::new();
        if rail {
            let tick = if major { "─" } else { "╴" };
            line.push(' ').styled(&format!("{scale:>3}"), p.fg).reset().styled(tick, if major { p.fg } else { p.dim }).reset().push(' ');
        } else {
            line.push_str("  ").styled(&format!("{scale:>3}"), p.dim).reset().push(' ');
        }
        for (b, band) in bands.iter().enumerate() {
            let is_sel = selected == Some(b);
            if faders == Faders::Dots {
                let k = if band.kind.uses_gain() { dot_of(band.gain) } else { travel / 2 };
                let (glyph, style) = led_cell(r, k, travel / 2, is_sel, band.kind.uses_gain(), p);
                center_cell(&mut line, &glyph.to_string(), style, 1, band_w);
                continue;
            }
            // Cuts have no gain axis — their knob sits on the 0 dB baseline.
            let knob_row = if band.kind.uses_gain() { row_of(band.gain) } else { center };
            let (glyph, style) = if r == knob_row {
                let col = if is_sel {
                    p.accent
                } else if band.kind.uses_gain() {
                    p.fg
                } else {
                    p.warn
                };
                (knob.as_str(), col)
            } else if (r > center && r < knob_row) || (r < center && r > knob_row) {
                if rail {
                    ("┃", if is_sel { p.accent } else { p.fg })
                } else {
                    ("│", if is_sel { p.accent } else { p.dim })
                }
            } else if r == center {
                (if rail { "┼" } else { "─" }, rail_style)
            } else if rail {
                ("│", rail_style)
            } else {
                (" ", Style::new())
            };
            center_cell(&mut line, glyph, style, 1, band_w);
        }
        out.push(line.into_line());
    }

    // Freq labels from the live band frequencies; selected in accent.
    let mut freq_line = LineBuf::new();
    freq_line.spaces(SCALE_W);
    for (b, band) in bands.iter().enumerate() {
        let label = format_freq(band.freq);
        let col = if selected == Some(b) { p.accent } else { p.dim };
        center_cell(&mut freq_line, &label, col, crate::tui::width(&label), band_w);
    }
    out.push(freq_line.into_line());

    // Filter-type row: PK/LS/HS/LC/HC. Peak (the default) stays dim so the
    // bands doing something unusual pop.
    let mut type_line = LineBuf::new();
    type_line.spaces(SCALE_W);
    for (b, band) in bands.iter().enumerate() {
        let label = band.kind.short_label();
        let col = if selected == Some(b) {
            p.accent
        } else if band.kind == BandType::Peak {
            p.dim
        } else {
            p.warn
        };
        center_cell(&mut type_line, label, col, crate::tui::width(label), band_w);
    }
    out.push(type_line.into_line());

    // Signed-dB values; cuts show "cut" (they have no gain axis). Whole dB
    // stays compact ("+6"); fine-stepped gains show their decimal ("+5.5").
    let mut db_line = LineBuf::new();
    db_line.spaces(SCALE_W);
    for (b, band) in bands.iter().enumerate() {
        let (val, active) = if band.kind.uses_gain() {
            let g = band.gain;
            let val = if (g - g.round()).abs() < 0.05 {
                format!("{:+}", g.round() as i32)
            } else {
                format!("{:+.1}", g)
            };
            (val, g.abs() >= 0.05)
        } else {
            ("cut".to_string(), true)
        };
        let col = if selected == Some(b) {
            p.accent
        } else if active {
            p.fg
        } else {
            p.dim
        };
        center_cell(&mut db_line, &val, col, crate::tui::width(&val), band_w);
    }
    out.push(db_line.into_line());
    out
}

/// How `eq_body` draws a band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Faders {
    /// The editor dialog's: the knob, and the stretch from 0 dB to it.
    Lines,
    /// HiFi's bars: a full-height rail (`│`, `┃` from 0 dB to the knob) and
    /// a scale with a tick per row (longer at ±12 and 0), the fixed parts
    /// brighter than the knobs' travel.
    Rails,
    /// HiFi's LED dot matrix: a column of braille dots, four a row, lit from
    /// 0 dB to the knob (a bright dot row), the rest dark ghosts; the
    /// selected band's ghosts glow. The scale as `Rails`.
    Dots,
}

/// A cell of a band's LED column (`Faders::Dots`): grid row `r`, the knob
/// at dot `k` and 0 dB at dot `zero`, counted from the top.
fn led_cell(r: usize, k: usize, zero: usize, selected: bool, gain: bool, p: &Palette) -> (char, Style) {
    const BITS: [u8; 4] = [0x01 | 0x08, 0x02 | 0x10, 0x04 | 0x20, 0x40 | 0x80];
    let (lo, hi) = if k < zero { (k + 1, zero) } else { (zero, k.saturating_sub(1)) };
    let (mut fill, mut knob) = (0u8, 0u8);
    for (i, bits) in BITS.iter().enumerate() {
        let d = 4 * r + i;
        if d == k {
            knob |= bits;
        } else if k != zero && d >= lo && d <= hi {
            fill |= bits;
        }
    }
    let glyph = |mask: u8| char::from_u32(0x2800 + mask as u32).unwrap_or(' ');
    if knob != 0 {
        (glyph(knob | fill), if !gain { p.warn } else { p.fg.patch(p.bold) })
    } else if fill != 0 {
        (glyph(fill), p.accent)
    } else if crate::term::no_color() {
        (' ', Style::new())
    } else {
        ('⣿', if selected { p.dim } else { p.rule })
    }
}

/// Render the EQ editor body: the header, the sliders and their labels
/// (`eq_body`), and the selected band's full parametric readout. The
/// dialog adds FX, crossfeed, balance and ReplayGain under it
/// (`ui::dsp_readouts`, HiFi's `ui_hifi::dsp_line`).
pub fn render_eq_screen(
    bands: &[BandSettings; EQ_BANDS],
    selected: usize,
    title: &str,
    knob: char,
    p: &Palette,
    height: usize,
) -> Vec<Line<'static>> {
    // Header, blank, sliders, three label rows, blank, readout.
    let slider_h = slider_rows(height.saturating_sub(7));
    let mut out = Vec::with_capacity(slider_h + 7);
    let mut header = LineBuf::new();
    header.push_str("  ").styled("E Q U A L I Z E R", p.accent.patch(p.bold)).reset().push_str("   ").styled(title, p.dim);
    out.push(header.into_line());
    out.push(Line::default());
    out.extend(eq_body(bands, Some(selected), knob, Faders::Lines, p, 6, slider_h));
    out.push(Line::default());
    out.push(band_readout(selected, &bands[selected], p));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_bands() -> [BandSettings; EQ_BANDS] {
        std::array::from_fn(BandSettings::inert)
    }

    #[test]
    fn eq_screen_has_header_labels_knobs_and_readouts() {
        let mut bands = flat_bands();
        bands[0].gain = 6.0; // boost the 31 Hz band
        bands[1].gain = 2.5; // fractional gain (fine-step editing)
        bands[9].gain = -6.0; // cut the 16k band
        let p = crate::theme::palette(crate::theme::ThemeKind::HiFi);
        let lines = render_eq_screen(&bands, 0, "Custom", '◆', p, 24);

        let joined = lines.iter().map(crate::tui::plain).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("E Q U A L I Z E R"));
        assert!(joined.contains('◆')); // a knob rendered
        assert!(joined.contains("16k")); // freq labels present
        assert!(joined.contains("+6")); // boosted band's dB value
        assert!(joined.contains("+12") && joined.contains("-12"), "the scale's top is signed");
        assert!(joined.contains("+2.5")); // fractional gain shown exactly, not rounded
        assert!(joined.contains("-6")); // cut band's dB value
        // FX, crossfeed, balance and ReplayGain are the player's: not here.
        for gone in ["FX", "XFEED", "BAL", "RG"] {
            assert!(!joined.contains(gone), "{gone} in the EQ editor");
        }
    }

    #[test]
    fn eq_screen_shows_parametric_band_details() {
        let mut bands = flat_bands();
        bands[2] = BandSettings {
            kind: BandType::LowShelf,
            freq: 105.0,
            gain: 5.5,
            q: 0.71,
        };
        bands[5] = BandSettings {
            kind: BandType::HighCut,
            freq: 10000.0,
            gain: 0.0,
            q: 0.71,
        };
        let p = crate::theme::palette(crate::theme::ThemeKind::Classic);
        let lines = render_eq_screen(&bands, 2, "Custom", '█', p, 26);
        let joined = lines.iter().map(crate::tui::plain).collect::<Vec<_>>().join("\n");

        // Selected-band readout: full parametric detail.
        assert!(joined.contains("BAND"), "readout line present");
        assert!(joined.contains("LOW SHELF"), "type spelled out");
        assert!(joined.contains("105 Hz"), "frequency shown");
        assert!(joined.contains("+5.5 dB"), "fractional gain shown");
        assert!(joined.contains("Q 0.71"), "Q shown");
        // Type row markers and the moved band's freq label.
        assert!(joined.contains("LS"), "type row shows shelf marker");
        assert!(joined.contains("HC"), "type row shows cut marker");
        assert!(joined.contains("10k"), "moved band labels its own freq");
        // Cut bands display "cut" instead of a dB value.
        assert!(joined.contains("cut"), "cut band has no dB value");
    }

    #[test]
    fn eq_screen_line_count_scales_with_height() {
        let bands = flat_bands();
        let p = crate::theme::palette(crate::theme::ThemeKind::Classic);
        let short = render_eq_screen(&bands, 0, "Flat", '█', p, 16);
        let tall = render_eq_screen(&bands, 0, "Flat", '█', p, 30);
        assert!(tall.len() > short.len());
    }

    #[test]
    fn hifis_faders_have_bright_rails_and_a_ticked_scale() {
        let mut bands = flat_bands();
        bands[0].gain = 12.0;
        let p = crate::theme::palette(crate::theme::ThemeKind::HiFi);
        let lines = eq_body(&bands, None, '●', Faders::Rails, p, 6, 9);
        let rows: Vec<String> = lines.iter().map(crate::tui::plain).collect();
        // A tick every row, long at +12, 0 and -12.
        assert!(rows[0].starts_with(" +12─"), "{:?}", rows[0]);
        assert!(rows[1].starts_with("    ╴"), "{:?}", rows[1]);
        assert!(rows[4].starts_with("   0─"), "{:?}", rows[4]);
        assert!(rows[8].starts_with(" -12─"), "{:?}", rows[8]);
        // Rails top to bottom, `┼` on 0 dB, `┃` from 0 dB up to a raised knob.
        assert_eq!(rows[0].matches('●').count(), 1);
        assert_eq!(rows[2].matches('┃').count(), 1);
        assert_eq!(rows[8].matches('│').count(), 10);
        // The fixed parts in the theme's mid tone, not its faintest.
        let rail = lines[8].spans.iter().find(|s| s.content.contains('│')).unwrap();
        assert_eq!(rail.style, p.dim);
    }

    #[test]
    fn hifis_led_faders_light_from_0_db_to_the_knob() {
        let mut bands = flat_bands();
        bands[0].gain = 12.0;
        bands[1].gain = -12.0;
        let p = crate::theme::palette(crate::theme::ThemeKind::HiFi);
        let lines = eq_body(&bands, Some(2), '●', Faders::Dots, p, 6, 9);
        // 36 dots, 0 dB on dot 17 (the fifth row's second): a flat band's
        // knob alone there, ghosts elsewhere.
        // A band's cell: centred in its 6 columns, after the scale.
        let col = |b: usize| -> Vec<(char, Style)> {
            let at = SCALE_W + b * 6 + 2;
            lines[..9].iter().map(|l| l.spans.iter().flat_map(|s| s.content.chars().map(move |c| (c, s.style))).nth(at).unwrap()).collect()
        };
        let flat = col(5);
        assert_eq!(flat[4], ('⠒', p.fg.patch(p.bold)));
        assert!(flat.iter().enumerate().all(|(r, &(g, s))| r == 4 || (g == '⣿' && s == p.rule)));
        // +12: lit from 0 dB up to the top dot, the knob there.
        let up = col(0);
        assert_eq!(up[0], ('⣿', p.fg.patch(p.bold)));
        assert!((1..4).all(|r| up[r] == ('⣿', p.accent)));
        assert_eq!(up[4], ('⠛', p.accent), "down to the 0 dB dot row");
        // -12: down to the last dot in use.
        let down = col(1);
        assert_eq!(down[8].1, p.fg.patch(p.bold));
        // The selected band's ghosts glow.
        assert!(col(2).iter().enumerate().all(|(r, &(_, s))| r == 4 || s == p.dim));
    }
}
