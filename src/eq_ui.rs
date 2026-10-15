//! EQ+FX editor screen — one shared, palette-driven renderer for the 10-band
//! parametric EQ, used by all three themes (like the library tree body). Pure:
//! it takes the band set + selection + readout strings and returns the screen
//! lines.

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
fn band_readout(selected: usize, b: &BandSettings, p: &Palette) -> Line<'static> {
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
pub fn headroom_line(h: &crate::eq::Headroom, preamp_db: f32, p: &Palette) -> Line<'static> {
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
    if let Some(s) = h.suggested_preamp {
        sep(&mut line);
        line.styled("[a]", p.accent).reset().push(' ').styled(&format!("set preamp {:+.1} dB", s), p.dim);
    }
    line.into_line()
}

/// Render the EQ editor body: a row of 10 vertical sliders (a knob at each
/// band's gain, a 0 dB baseline, the selected band highlighted), freq / type /
/// signed-dB labels beneath, the selected band's full parametric readout, and a
/// spatial/FX readout row. Cut bands (no gain axis) park their knob on the
/// baseline in the warn color; the type row tells them apart. `knob` is the
/// theme's glyph (● / ◆ / █).
#[allow(clippy::too_many_arguments)]
pub fn render_eq_screen(
    bands: &[BandSettings; EQ_BANDS],
    selected: usize,
    title: &str,
    readouts: &[(&str, &str)],
    knob: char,
    p: &Palette,
    _width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    let band_w = 6;
    // Odd slider height so there's a true centre (0 dB) row — rounded DOWN:
    // `| 1` rounded an even budget up, one row past what the caller had.
    let slider_h = {
        let h = height.saturating_sub(9).clamp(5, 15);
        if h.is_multiple_of(2) { h - 1 } else { h }
    };
    let center = slider_h / 2;

    // gain (dB) → grid row (0 = top = +limit … slider_h-1 = bottom = -limit).
    let row_of = |g: f32| -> usize {
        let t = (EQ_GAIN_LIMIT - g) / (2.0 * EQ_GAIN_LIMIT);
        (t * (slider_h - 1) as f32).round().clamp(0.0, (slider_h - 1) as f32) as usize
    };

    let mut out = Vec::with_capacity(slider_h + 8);

    // Header.
    let mut header = LineBuf::new();
    header.push_str("  ").styled("E Q U A L I Z E R", p.accent.patch(p.bold)).reset().push_str("   ").styled(title, p.dim);
    out.push(header.into_line());
    out.push(Line::default());

    // Slider grid.
    let knob = knob.to_string();
    for r in 0..slider_h {
        let scale = if r == 0 {
            format!("{:>3}", EQ_GAIN_LIMIT as i32)
        } else if r == center {
            "  0".to_string()
        } else if r == slider_h - 1 {
            format!("{:>3}", -(EQ_GAIN_LIMIT as i32))
        } else {
            "   ".to_string()
        };
        let mut line = LineBuf::new();
        line.push_str("  ").styled(&scale, p.dim).reset().push(' ');
        for (b, band) in bands.iter().enumerate() {
            let is_sel = b == selected;
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
            } else if r == center {
                ("─", p.rule)
            } else if (r > center && r < knob_row) || (r < center && r > knob_row) {
                ("│", if is_sel { p.accent } else { p.dim })
            } else {
                (" ", Style::new())
            };
            center_cell(&mut line, glyph, style, 1, band_w);
        }
        out.push(line.into_line());
    }

    // Freq labels from the live band frequencies; selected in accent.
    let mut freq_line = LineBuf::new();
    freq_line.spaces(6);
    for (b, band) in bands.iter().enumerate() {
        let label = format_freq(band.freq);
        let col = if b == selected { p.accent } else { p.dim };
        center_cell(&mut freq_line, &label, col, crate::tui::width(&label), band_w);
    }
    out.push(freq_line.into_line());

    // Filter-type row: PK/LS/HS/LC/HC. Peak (the default) stays dim so the
    // bands doing something unusual pop.
    let mut type_line = LineBuf::new();
    type_line.spaces(6);
    for (b, band) in bands.iter().enumerate() {
        let label = band.kind.short_label();
        let col = if b == selected {
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
    db_line.spaces(6);
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
        let col = if b == selected {
            p.accent
        } else if active {
            p.fg
        } else {
            p.dim
        };
        center_cell(&mut db_line, &val, col, crate::tui::width(&val), band_w);
    }
    out.push(db_line.into_line());

    // Selected band's full parametric readout.
    out.push(Line::default());
    out.push(band_readout(selected, &bands[selected], p));

    out.push(Line::default());

    // FX / spatial readouts.
    let mut fx = LineBuf::new();
    fx.push_str("  ");
    for (i, (label, value)) in readouts.iter().enumerate() {
        if i > 0 {
            fx.reset().push_str("  ").styled("·", p.rule).reset().push_str("  ");
        }
        fx.styled(label, p.dim).reset().push(' ').styled(value, p.fg);
    }
    out.push(fx.into_line());

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
        let readouts = [("FX", "None"), ("XFEED", "Off"), ("BAL", "C"), ("RG", "Track")];
        let lines = render_eq_screen(&bands, 0, "Custom", &readouts, '◆', p, 80, 24);

        let joined = lines.iter().map(crate::tui::plain).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("E Q U A L I Z E R"));
        assert!(joined.contains('◆')); // a knob rendered
        assert!(joined.contains("16k")); // freq labels present
        assert!(joined.contains("+6")); // boosted band's dB value
        assert!(joined.contains("+2.5")); // fractional gain shown exactly, not rounded
        assert!(joined.contains("-6")); // cut band's dB value
        assert!(joined.contains("XFEED") && joined.contains("Track")); // readouts
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
        let lines = render_eq_screen(&bands, 2, "Custom", &[], '█', p, 80, 26);
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
        let short = render_eq_screen(&bands, 0, "Flat", &[], '█', p, 80, 16);
        let tall = render_eq_screen(&bands, 0, "Flat", &[], '█', p, 80, 30);
        assert!(tall.len() > short.len());
    }
}
