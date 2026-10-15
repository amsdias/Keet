//! The lyrics panel: the current track's lyrics, drawn into a box (the
//! floating panel over the player, or a docked one), following the song
//! when they are synced. One layout for every theme; each keeps its own line
//! style — Classic with timestamps, Minimal centred, HiFi in capitals.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;

use crate::state::{PlayerState, UiState, ViewMode};
use crate::theme::ThemeKind;
use crate::tui::{self, LineBuf};
use crate::ui::{format_time, Ctx};

/// The panel's title for the box: where the lyrics came from, and the sync
/// offset when there is one.
pub fn title(kind: ThemeKind, ui: &UiState) -> String {
    let upper = |s: String| if kind == ThemeKind::HiFi { s.to_uppercase() } else { s };
    let mut t = upper(String::from("Lyrics"));
    if let Some(l) = ui.lyrics() {
        t.push_str(" · ");
        t.push_str(&upper(crate::lyrics::source_line(l, ui.lyrics_source)));
        if l.is_synced() && ui.lyrics_offset != 0.0 {
            // The unit stays lower case in HiFi's capitals, as in the footer.
            t.push_str(&format!(" · {} {:+.1} s", upper("offset".into()), ui.lyrics_offset));
        }
    }
    t
}

/// Draw the panel inside `area`: the lines, then the footer (the keys,
/// wrapped onto as many rows as they need; a status message goes in a
/// floating panel's border, `ui::border_status`).
pub fn draw(f: &mut Frame, cx: &mut Ctx, area: Rect) {
    let kind = cx.state.theme_kind();
    let p = crate::theme::palette(kind);
    if area.height < 2 || area.width < 4 {
        return;
    }
    let ui = &mut *cx.ui;
    let width = area.width as usize;
    let synced = ui.lyrics().is_some_and(|l| l.is_synced());
    let upper = kind == ThemeKind::HiFi;
    let label = |s: &str| if upper { s.to_uppercase() } else { s.to_string() };
    let look = crate::ui::key_look(kind, p);
    let footer_of = |ui: &UiState, follow: bool| -> Vec<crate::hints::Bar> {
        // The way in and out first: a short panel drops the footer's LAST
        // rows, and those must never be the only way out.
        let mut keys = crate::ui::panel_footer_keys(ui, ViewMode::Lyrics, upper);
        keys.push(("w/s", label("scroll"), None));
        if follow {
            // Scrolled away: the way back to the song.
            keys.push(("↵", label("follow"), None));
        }
        if synced {
            // The unit stays lower case in HiFi's capitals ("0.5 S" reads as
            // something else).
            keys.push(("a/d", label("sync ±0.5").to_string() + " s", None));
            keys.push(("0", label("reset"), None));
        }
        crate::hints::bar_lines(&keys, &look, width.saturating_sub(2)).into_iter().map(|b| crate::hints::indent(b, 2)).collect()
    };
    // Where the lyrics stand is settled BEFORE the footer, which offers
    // `↵ follow` by it (built after, the hint came a frame late), for both
    // bodies: following, the footer has no hint; scrolled, it has one, and
    // it can wrap onto another row — a step is clamped to that body, or the
    // last lyric row could never be reached. Each footer is built only when
    // it can be the one drawn: the plain one while following (or for plain
    // lyrics), the hinted one once scrolled or about to be.
    let following = !synced || ui.lyrics_auto_scroll;
    let plain = following.then(|| footer_of(ui, false));
    let hinted = (synced && (!ui.lyrics_auto_scroll || ui.lyrics_step != 0)).then(|| footer_of(ui, true));
    let rows_under = |footer: &Option<Vec<crate::hints::Bar>>| footer.as_deref().map(|f| body_of(area, f).height as usize);
    let scrolled_rows = rows_under(&hinted).or(rows_under(&plain)).unwrap_or(area.height as usize - 1);
    let following_rows = rows_under(&plain).unwrap_or(scrolled_rows);
    let current = settle(cx.state, ui, kind, width, following_rows, scrolled_rows);
    let footer = match (plain, hinted) {
        (_, Some(hinted)) if synced && !ui.lyrics_auto_scroll => hinted,
        (Some(plain), _) => plain,
        (None, hinted) => hinted.unwrap_or_default(),
    };
    let body = body_of(area, &footer);
    let footer_h = area.height - body.height;
    let (lines, scroll) = lines(ui, kind, p, current, body.height as usize);
    tui::draw_lines(f.buffer_mut(), body, &lines);
    if let Some((total, first)) = scroll {
        tui::scrollbar(f, body, total, first, p.accent, p.rule);
    }
    for (i, bar) in footer.iter().take(footer_h as usize).enumerate() {
        let y = body.bottom() + i as u16;
        bar.register(&mut ui.hits, area.x, y);
        tui::draw_lines(f.buffer_mut(), Rect { y, height: 1, ..area }, std::slice::from_ref(&bar.line));
    }
}

/// The panel's body above `footer`: the footer takes the rows it needs,
/// leaving the lyrics at least one. One place for it, used to settle the
/// scroll and to draw (two copies could disagree, and the last lyric row
/// went out of reach).
fn body_of(area: Rect, footer: &[crate::hints::Bar]) -> Rect {
    let footer_h = (footer.len() as u16).min(area.height - 1);
    Rect { height: area.height - footer_h, ..area }
}

/// The lyrics wrapped for the panel, kept in `UiState::lyrics_wrap` until
/// the width or the theme change (new lyrics drop it, `set_lyrics`):
/// rewrapping the whole song every frame — 30 a second while a
/// visualization moves — allocated a row for every line to show a dozen.
pub struct Wrapped {
    width: usize,
    kind: ThemeKind,
    /// Each lyric line's rows.
    parts: Vec<Vec<String>>,
    /// Minimal's left padding for each row, centring it (measured once).
    pads: Vec<Vec<usize>>,
    /// Each lyric line's first screen row: where following the song puts
    /// it, and (searched) which line a screen row belongs to.
    first_row: Vec<usize>,
    /// Screen rows in all.
    total: usize,
    /// Classic's timestamps, formatted once.
    times: Vec<Option<String>>,
}

impl Wrapped {
    /// The (lyric line, row of it) shown on screen row `row`; past the end,
    /// the last row.
    fn line_at(&self, row: usize) -> (usize, usize) {
        let row = row.min(self.total.saturating_sub(1));
        let line = self.first_row.partition_point(|&f| f <= row).saturating_sub(1);
        (line, row - self.first_row.get(line).copied().unwrap_or(0))
    }

    /// The first screen row when following the song in `rows` rows: the
    /// current line's first row centred; the top before the first line (none
    /// is current yet: it kept wherever the user had scrolled).
    fn follow(&self, current: Option<usize>, rows: usize) -> usize {
        current.and_then(|c| self.first_row.get(c)).map_or(0, |&row| row.saturating_sub(rows / 2))
    }

    /// The screen row of row `n` of lyric `line` (its last if it has fewer).
    fn row_of(&self, (line, n): (usize, usize)) -> usize {
        match self.parts.get(line) {
            Some(rows) => self.first_row[line] + n.min(rows.len().saturating_sub(1)),
            None => self.total.saturating_sub(1),
        }
    }
}

fn wrap_all(lyrics: &crate::lyrics::Lyrics, text_w: usize, kind: ThemeKind) -> Wrapped {
    let times: Vec<Option<String>> = (0..lyrics.line_count())
        .map(|i| if kind == ThemeKind::Classic { lyrics.line_time(i).map(format_time) } else { None })
        .collect();
    let parts: Vec<Vec<String>> = (0..lyrics.line_count())
        .map(|i| {
            let text = lyrics.line_text(i);
            match kind {
                // Continuation rows sit under the text: "  mm:ss   ".
                ThemeKind::Classic => {
                    let lead = 2 + times[i].as_ref().map_or(0, |t| tui::width(t) + 3);
                    tui::wrap(text, text_w.saturating_sub(lead))
                }
                ThemeKind::Minimal => tui::wrap(text, text_w.saturating_sub(4)),
                ThemeKind::HiFi => tui::wrap(&text.to_uppercase(), text_w.saturating_sub(6)),
            }
        })
        .collect();
    let pads = match kind {
        ThemeKind::Minimal => parts
            .iter()
            .map(|rows| rows.iter().map(|r| text_w.saturating_sub(tui::width(r)) / 2).collect())
            .collect(),
        _ => Vec::new(),
    };
    let mut total = 0;
    let first_row = parts
        .iter()
        .map(|rows| {
            let first = total;
            total += rows.len();
            first
        })
        .collect();
    Wrapped { width: text_w, kind, parts, pads, first_row, total, times }
}

/// Settle where the lyrics stand this frame for a `width`×`rows` space: the
/// wrap for the width (a rewrap keeps the verse the user scrolled to), the
/// song's place when following, then the user's scrolling since the last
/// frame, which stops following if it moves anything. `following_rows` and
/// `scrolled_rows` are the body's rows either way (the `↵ follow` hint can
/// take one). Writes the scroll and whether it follows to `ui`; returns the
/// current line it went by (the clock is read once a frame).
fn settle(
    state: &PlayerState,
    ui: &mut UiState,
    kind: ThemeKind,
    width: usize,
    following_rows: usize,
    scrolled_rows: usize,
) -> Option<usize> {
    let clock = state.time_secs() + ui.lyrics_offset;
    let mut scroll = ui.lyrics_scroll;
    let mut auto = ui.lyrics_auto_scroll;
    let step = std::mem::take(&mut ui.lyrics_step);
    let (lyrics, wrap) = ui.lyrics_and_wrap();
    let lyrics = lyrics?;
    let synced = lyrics.is_synced();
    let current = lyrics.current_line(clock);
    // A column for the scrollbar on the right.
    let text_w = width.saturating_sub(1);
    if wrap.as_ref().is_none_or(|w| w.width != text_w || w.kind != kind) {
        let new = wrap_all(lyrics, text_w, kind);
        // A rewrap (a resize, docking, a theme) keeps the verse the user
        // scrolled to on screen: the same lyric row, wherever it now lands.
        if let Some(old) = wrap.as_ref() {
            scroll = new.row_of(old.line_at(scroll));
        }
        *wrap = Some(new);
    }
    let wrapped = wrap.as_ref().expect("just filled");
    let rows = if synced && auto { following_rows } else { scrolled_rows };
    if synced && auto {
        scroll = wrapped.follow(current, rows);
    }
    // The user's scrolling since the last frame, from where the song put
    // the panel; it stops following the song — if it moved anything (lyrics
    // that fit the panel cannot move, and must go on following once the
    // panel is smaller). Clamped to the body it leaves: scrolled.
    scroll = scroll.min(wrapped.total.saturating_sub(rows));
    let to = scroll.saturating_add_signed(step).min(wrapped.total.saturating_sub(scrolled_rows));
    if to != scroll {
        scroll = to;
        auto = false;
    }
    ui.lyrics_scroll = scroll;
    ui.lyrics_auto_scroll = auto;
    current
}

/// The lyrics for `rows` rows from where `settle` left them (no second look
/// at the clock: it could fall on the next line and draw another place than
/// the one settled), each line wrapped to the width (cut, a long line was
/// unreadable in a narrow panel); and (rows in all, first shown) for the
/// scrollbar. Only the rows shown are styled. (Following, the footer has no
/// hint, so `rows` are the rows `settle` had; scrolled, a hint row only
/// leaves more to scroll.)
fn lines(
    ui: &mut UiState,
    kind: ThemeKind,
    p: &crate::theme::Palette,
    current: Option<usize>,
    rows: usize,
) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let looking = ui.lyrics_receiver.is_some();
    let mut scroll = ui.lyrics_scroll;
    let (lyrics, wrap) = ui.lyrics_and_wrap();
    let (Some(_), Some(wrapped)) = (lyrics, wrap.as_ref()) else {
        let unavailable = ui.lyrics_unavailable.map(|why| format!("lyrics unavailable: {why}"));
        let what = if looking { "looking for lyrics…" } else { unavailable.as_deref().unwrap_or("no lyrics for this track") };
        let what = if kind == ThemeKind::HiFi { what.to_uppercase() } else { what.to_string() };
        return (vec![Line::default(), tui::line(format!("    {what}"), p.dim)], None);
    };
    let total = wrapped.total;
    scroll = scroll.min(total.saturating_sub(rows));
    let shown_rows = rows.min(total - scroll);
    let (first, first_n) = wrapped.line_at(scroll);
    let shown = (first..wrapped.parts.len())
        .flat_map(|i| (if i == first { first_n } else { 0 }..wrapped.parts[i].len()).map(move |n| (i, n)))
        .take(shown_rows)
        .map(|(i, n)| {
            let part = &wrapped.parts[i][n];
            let is_current = current == Some(i);
            let past = current.is_some_and(|c| i < c);
            let mut l = LineBuf::new();
            match kind {
                ThemeKind::Classic => {
                    l.push_str("  ");
                    match (&wrapped.times[i], n) {
                        (Some(t), 0) if is_current => {
                            l.styled(t, p.accent).reset().push(' ').styled(&format!("▸ {part}"), p.accent.patch(p.bold));
                        }
                        (Some(t), 0) if past => {
                            l.styled(&format!("{t}   {part}"), p.dim);
                        }
                        (Some(t), 0) => {
                            l.styled(t, p.dim).reset().push_str("   ").push_str(part);
                        }
                        (Some(t), _) => {
                            let style = if is_current { p.accent.patch(p.bold) } else if past { p.dim } else { p.fg };
                            l.spaces(tui::width(t) + 3).styled(part, style);
                        }
                        (None, _) => {
                            l.push_str(part);
                        }
                    }
                }
                ThemeKind::Minimal => {
                    // Centred; the current line in bold accent, the rest plain.
                    let style = if is_current { p.bold.patch(p.accent) } else { p.fg };
                    l.spaces(wrapped.pads[i][n]).styled(part, style);
                }
                ThemeKind::HiFi => {
                    let style = if is_current { p.accent.patch(p.bold) } else { p.dim };
                    let mark = if is_current && n == 0 { "▸ " } else { "  " };
                    l.spaces(2).styled(&format!("{mark}{part}"), style);
                }
            }
            l.into_line()
        })
        .collect();
    ui.lyrics_scroll = scroll;
    (shown, Some((total, scroll)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hifis_capitals_leave_the_offsets_unit_alone() {
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics("[00:01.00]one")));
        ui.lyrics_offset = 0.5;
        let t = title(ThemeKind::HiFi, &ui);
        assert!(t.starts_with("LYRICS · "), "{t}");
        assert!(t.ends_with("OFFSET +0.5 s"), "the unit as the footer writes it: {t}");
        assert!(title(ThemeKind::Classic, &ui).ends_with("offset +0.5 s"));
    }
}
