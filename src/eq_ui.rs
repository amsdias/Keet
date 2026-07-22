//! EQ+FX editor screen — one shared, palette-driven renderer for the 10-band
//! parametric EQ, used by all three themes (like the library tree body). Pure:
//! it takes the band set + selection + readout strings and returns the screen
//! lines.

use crate::eq::{format_freq, BandSettings, BandType, EQ_BANDS, EQ_GAIN_LIMIT};
use crate::theme::Palette;

/// Pad `s` (a single visible glyph, possibly with SGR codes) to `w` columns,
/// centred. `vis` is its visible width (1 for our glyphs).
fn center_cell(s: &str, vis: usize, w: usize) -> String {
    let pad = w.saturating_sub(vis);
    let lp = pad / 2;
    format!("{}{}{}", " ".repeat(lp), s, " ".repeat(pad - lp))
}

/// Long-form band description for the selected-band readout, e.g.
/// `BAND 3 · LOW SHELF · 105 Hz · +5.5 dB · Q 0.71` (gain omitted for cuts).
fn band_readout(selected: usize, b: &BandSettings, p: &Palette) -> String {
    let rst = p.reset;
    let kind = match b.kind {
        BandType::Peak => "PEAK",
        BandType::LowShelf => "LOW SHELF",
        BandType::HighShelf => "HIGH SHELF",
        BandType::LowCut => "LOW CUT",
        BandType::HighCut => "HIGH CUT",
    };
    let freq = if b.freq >= 1000.0 {
        format!("{:.1} kHz", b.freq / 1000.0)
    } else {
        format!("{:.0} Hz", b.freq)
    };
    let sep = format!(" {}·{} ", p.rule, rst);
    let mut line = format!(
        "  {dim}BAND{rst} {acc}{n}{rst}{sep}{fg}{kind}{rst}{sep}{fg}{freq}{rst}",
        dim = p.dim, acc = p.accent, fg = p.fg, rst = rst,
        n = selected + 1, sep = sep, kind = kind, freq = freq,
    );
    if b.kind.uses_gain() {
        line.push_str(&format!("{sep}{fg}{:+.1} dB{rst}", b.gain, fg = p.fg, rst = rst));
    }
    line.push_str(&format!("{sep}{fg}Q {:.2}{rst}", b.q, fg = p.fg, rst = rst));
    line
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
) -> Vec<String> {
    let rst = p.reset;
    let band_w = 6;
    // Odd slider height so there's a true centre (0 dB) row.
    let slider_h = (height.saturating_sub(9).clamp(5, 15)) | 1;
    let center = slider_h / 2;

    // gain (dB) → grid row (0 = top = +limit … slider_h-1 = bottom = -limit).
    let row_of = |g: f32| -> usize {
        let t = (EQ_GAIN_LIMIT - g) / (2.0 * EQ_GAIN_LIMIT);
        (t * (slider_h - 1) as f32).round().clamp(0.0, (slider_h - 1) as f32) as usize
    };

    let mut out = Vec::with_capacity(slider_h + 8);

    // Header.
    out.push(format!(
        "  {}{}E Q U A L I Z E R{}   {}{}{}",
        p.accent, p.bold, rst, p.dim, title, rst
    ));
    out.push(String::new());

    // Slider grid.
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
        let mut line = format!("  {}{}{} ", p.dim, scale, rst);
        for (b, band) in bands.iter().enumerate() {
            let is_sel = b == selected;
            // Cuts have no gain axis — their knob sits on the 0 dB baseline.
            let knob_row = if band.kind.uses_gain() { row_of(band.gain) } else { center };
            let cell = if r == knob_row {
                let col = if is_sel {
                    p.accent
                } else if band.kind.uses_gain() {
                    p.fg
                } else {
                    p.warn
                };
                format!("{col}{knob}{rst}")
            } else if r == center {
                format!("{}─{}", p.rule, rst)
            } else if (r > center && r < knob_row) || (r < center && r > knob_row) {
                let col = if is_sel { p.accent } else { p.dim };
                format!("{col}│{rst}")
            } else {
                " ".to_string()
            };
            line.push_str(&center_cell(&cell, 1, band_w));
        }
        out.push(line);
    }

    // Freq labels from the live band frequencies; selected in accent.
    let mut freq_line = "      ".to_string();
    for (b, band) in bands.iter().enumerate() {
        let label = format_freq(band.freq);
        let colored = if b == selected {
            format!("{}{}{}", p.accent, label, rst)
        } else {
            format!("{}{}{}", p.dim, label, rst)
        };
        freq_line.push_str(&center_cell(&colored, label.chars().count(), band_w));
    }
    out.push(freq_line);

    // Filter-type row: PK/LS/HS/LC/HC. Peak (the default) stays dim so the
    // bands doing something unusual pop.
    let mut type_line = "      ".to_string();
    for (b, band) in bands.iter().enumerate() {
        let label = band.kind.short_label();
        let col = if b == selected {
            p.accent
        } else if band.kind == BandType::Peak {
            p.dim
        } else {
            p.warn
        };
        type_line.push_str(&center_cell(
            &format!("{col}{label}{rst}"),
            label.chars().count(),
            band_w,
        ));
    }
    out.push(type_line);

    // Signed-dB values; cuts show "cut" (they have no gain axis).
    let mut db_line = "      ".to_string();
    for (b, band) in bands.iter().enumerate() {
        let (val, active) = if band.kind.uses_gain() {
            (format!("{:+}", band.gain.round() as i32), band.gain.round() as i32 != 0)
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
        db_line.push_str(&center_cell(&format!("{col}{val}{rst}"), val.chars().count(), band_w));
    }
    out.push(db_line);

    // Selected band's full parametric readout.
    out.push(String::new());
    out.push(band_readout(selected, &bands[selected], p));

    out.push(String::new());

    // FX / spatial readouts.
    let mut fx = String::from("  ");
    for (i, (label, value)) in readouts.iter().enumerate() {
        if i > 0 {
            fx.push_str(&format!("  {}·{}  ", p.rule, rst));
        }
        fx.push_str(&format!("{}{}{} {}{}{}", p.dim, label, rst, p.fg, value, rst));
    }
    out.push(fx);

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
        bands[9].gain = -6.0; // cut the 16k band
        let p = crate::theme::palette(crate::theme::ThemeKind::HiFi);
        let readouts = [("FX", "None"), ("XFEED", "Off"), ("BAL", "C"), ("RG", "Track")];
        let lines = render_eq_screen(&bands, 0, "Custom", &readouts, '◆', p, 80, 24);

        let joined = lines.join("\n");
        assert!(joined.contains("E Q U A L I Z E R"));
        assert!(joined.contains('◆')); // a knob rendered
        assert!(joined.contains("16k")); // freq labels present
        assert!(joined.contains("+6")); // boosted band's dB value
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
        let joined = lines.join("\n");

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
