//! EQ+FX editor screen — one shared, palette-driven renderer for the 10-band
//! graphic EQ, used by all three themes (like the library tree body). Pure: it
//! takes the gains + selection + readout strings and returns the screen lines.

use crate::eq::{EQ_BANDS, EQ_FREQ_LABELS, EQ_GAIN_LIMIT};
use crate::theme::Palette;

/// Pad `s` (a single visible glyph, possibly with SGR codes) to `w` columns,
/// centred. `vis` is its visible width (1 for our glyphs).
fn center_cell(s: &str, vis: usize, w: usize) -> String {
    let pad = w.saturating_sub(vis);
    let lp = pad / 2;
    format!("{}{}{}", " ".repeat(lp), s, " ".repeat(pad - lp))
}

/// Render the EQ editor body: a row of 10 vertical sliders (a knob at each band's
/// gain, a 0 dB baseline, the selected band highlighted), freq + signed-dB labels
/// beneath, and a spatial/FX readout row. `knob` is the theme's glyph (● / ◆ / █).
#[allow(clippy::too_many_arguments)]
pub fn render_eq_screen(
    gains: &[f32; EQ_BANDS],
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
    let slider_h = (height.saturating_sub(7).clamp(5, 15)) | 1;
    let center = slider_h / 2;

    // gain (dB) → grid row (0 = top = +limit … slider_h-1 = bottom = -limit).
    let row_of = |g: f32| -> usize {
        let t = (EQ_GAIN_LIMIT - g) / (2.0 * EQ_GAIN_LIMIT);
        (t * (slider_h - 1) as f32).round().clamp(0.0, (slider_h - 1) as f32) as usize
    };

    let mut out = Vec::with_capacity(slider_h + 6);

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
        for (b, &g) in gains.iter().enumerate() {
            let knob_row = row_of(g);
            let is_sel = b == selected;
            let cell = if r == knob_row {
                let col = if is_sel { p.accent } else { p.fg };
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

    // Freq labels; the selected band's label in accent.
    let mut freq_line = "      ".to_string();
    for (b, label) in EQ_FREQ_LABELS.iter().enumerate() {
        let colored = if b == selected {
            format!("{}{}{}", p.accent, label, rst)
        } else {
            format!("{}{}{}", p.dim, label, rst)
        };
        freq_line.push_str(&center_cell(&colored, label.len(), band_w));
    }
    out.push(freq_line);

    // Signed-dB values; selected in accent, non-zero in fg, zero dim.
    let mut db_line = "      ".to_string();
    for (b, &g) in gains.iter().enumerate() {
        let val = format!("{:+}", g.round() as i32);
        let col = if b == selected {
            p.accent
        } else if g.round() as i32 != 0 {
            p.fg
        } else {
            p.dim
        };
        db_line.push_str(&center_cell(&format!("{col}{val}{rst}"), val.chars().count(), band_w));
    }
    out.push(db_line);

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

    #[test]
    fn eq_screen_has_header_labels_knobs_and_readouts() {
        let mut gains = [0.0f32; EQ_BANDS];
        gains[0] = 6.0; // boost the 31 Hz band
        gains[9] = -6.0; // cut the 16k band
        let p = crate::theme::palette(crate::theme::ThemeKind::HiFi);
        let readouts = [("FX", "None"), ("XFEED", "Off"), ("BAL", "C"), ("RG", "Track")];
        let lines = render_eq_screen(&gains, 0, "Custom", &readouts, '◆', p, 80, 24);

        let joined = lines.join("\n");
        assert!(joined.contains("E Q U A L I Z E R"));
        assert!(joined.contains('◆')); // a knob rendered
        assert!(joined.contains("16k")); // freq labels present
        assert!(joined.contains("+6")); // boosted band's dB value
        assert!(joined.contains("-6")); // cut band's dB value
        assert!(joined.contains("XFEED") && joined.contains("Track")); // readouts
    }

    #[test]
    fn eq_screen_line_count_scales_with_height() {
        let gains = [0.0f32; EQ_BANDS];
        let p = crate::theme::palette(crate::theme::ThemeKind::Classic);
        let short = render_eq_screen(&gains, 0, "Flat", &[], '█', p, 80, 16);
        let tall = render_eq_screen(&gains, 0, "Flat", &[], '█', p, 80, 30);
        assert!(tall.len() > short.len());
    }
}
