//! The playlist panel: the flat playlist and the artist → album tree, drawn
//! into a box (the floating panel over the player, or a docked one). One
//! layout for every theme; each theme keeps its own row style, colours and
//! key-bar look.
//!
//! With room (`TWO_PANES` columns inside) both show side by side, the list
//! on the left; Tab moves the keys between them (`ui.library_tree_mode` is
//! the tree having them), and the pane with the keys has its heading lit.
//! Narrower, one pane, and Tab switches it, as the old view did.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;

use crate::ansi::truncate_plain;
use crate::state::{Hit, InputMode, UiState};
use crate::theme::{Palette, ThemeKind};
use crate::tui::{self, LineBuf};
use crate::ui::{format_time, list_scroll, Ctx};

/// Columns inside the box from which the list and the tree both show.
pub const TWO_PANES: u16 = 110;

/// The panel's title for the box: what it is, how many tracks, how long.
pub fn title(kind: ThemeKind, ui: &UiState, tracks: usize) -> String {
    // The rows' format (one copy of it: `ui::format_time`).
    let total = crate::ui::format_time(ui.metadata_cache.total_duration());
    match kind {
        ThemeKind::Classic => format!("Playlist · {tracks} tracks · {total}"),
        ThemeKind::Minimal => format!("Library · {tracks} tracks · {total}"),
        ThemeKind::HiFi => format!("LIBRARY · {tracks} TRK · {total}"),
    }
}

/// Draw the panel inside `area` (the box's inside): the panes, then a
/// footer with the prompt or the keys (a status message goes in a floating
/// panel's border, `ui::border_status`).
pub fn draw(f: &mut Frame, cx: &mut Ctx, area: Rect) {
    let kind = cx.state.theme_kind();
    let p = crate::theme::palette(kind);
    if area.height < 2 || area.width < 4 {
        return;
    }
    let two = area.width >= TWO_PANES;
    // The footer first: it takes the rows its keys need (wrapped when the
    // panel is narrow), leaving the panes at least two.
    let footer = footer(cx.ui, kind, p, area.width as usize, two);
    let footer_h = (footer.len() as u16).min(area.height.saturating_sub(2)).max(1);
    let body = Rect { height: area.height - footer_h, ..area };
    let tree = cx.ui.library_tree_mode;
    if two {
        // The list takes about three fifths; a rule column between.
        let list_w = area.width * 3 / 5;
        let list = Rect { width: list_w, ..body };
        let rule = Rect { x: body.x + list_w, width: 1, ..body };
        let tree_area = Rect { x: body.x + list_w + 2, width: body.width.saturating_sub(list_w + 2), ..body };
        flat_pane(f, cx, kind, p, list, Some(!tree));
        let col: Vec<Line> = (0..rule.height).map(|_| tui::line("│", p.rule)).collect();
        tui::draw_lines(f.buffer_mut(), rule, &col);
        tree_pane(f, cx.ui, kind, p, tree_area, Some(tree));
    } else if tree {
        tree_pane(f, cx.ui, kind, p, body, None);
    } else {
        flat_pane(f, cx, kind, p, body, None);
    }
    for (i, row) in footer.into_iter().take(footer_h as usize).enumerate() {
        let at = Rect { y: body.bottom() + i as u16, height: 1, ..area };
        match row {
            FooterRow::Keys(bar) => {
                bar.register(&mut cx.ui.hits, at.x, at.y);
                tui::draw_lines(f.buffer_mut(), at, &[bar.line]);
            }
            FooterRow::Field(line, col) => {
                // The terminal's cursor only while the field has the keys
                // (left open while another panel has them). `focus`, not
                // `view_mode`: a docked panel is drawn inside the theme,
                // where `view_mode` is the player's.
                if cx.ui.focus == crate::state::ViewMode::Playlist {
                    let screen = f.area();
                    crate::ui::show_cursor(f, (at.x - screen.x) as usize + col, (at.y - screen.y) as usize);
                }
                tui::draw_lines(f.buffer_mut(), at, &[line]);
            }
        }
    }
}

/// A row of the panel's footer: keys (clickable), or the field being typed
/// with the cursor's column.
enum FooterRow {
    Keys(crate::hints::Bar),
    Field(Line<'static>, usize),
}

/// A pane's heading: lit when the pane has the keys (two panes), dim
/// otherwise.
fn heading_style(p: &Palette, focused: Option<bool>) -> ratatui::style::Style {
    match focused {
        Some(true) => p.accent.patch(p.bold),
        _ => p.dim,
    }
}

/// The flat playlist in `area`: a column heading, then the rows.
/// `focused`: None with one pane, else whether this pane has the keys.
fn flat_pane(f: &mut Frame, cx: &mut Ctx, kind: ThemeKind, p: &Palette, area: Rect, focused: Option<bool>) {
    let playlist = cx.playlist;
    let ui = &mut *cx.ui;
    let width = area.width as usize;
    let rows = (area.height as usize).saturating_sub(1).max(1);
    ui.last_visible_rows = rows;
    let upper = kind == ThemeKind::HiFi;
    let num_w = playlist.len().to_string().len().max(2);
    let time_w = format_time(ui.metadata_cache.max_duration()).len().max(5);
    // marker, number, title, album (Classic and Minimal, when wide), time,
    // a column of margin and the scrollbar's.
    let fixed = 2 + 1 + 2 + num_w + 2 + 2 + time_w + 2;
    let content = width.saturating_sub(fixed);
    let album_w = if !upper && content >= 50 { (content * 30 / 100).clamp(12, 32) } else { 0 };
    let title_w = content.saturating_sub(album_w);
    let cell = |s: &str, w: usize| {
        let t = truncate_plain(s, w.saturating_sub(1));
        let pad = w.saturating_sub(tui::width(&t));
        format!("{t}{}", " ".repeat(pad))
    };
    let heading = if upper {
        format!("     {:>num_w$}  {}  {:>time_w$}", "#", cell("TITLE", title_w), "TIME")
    } else {
        let (t, a, d) = if kind == ThemeKind::Minimal { ("TITLE", "ALBUM", "TIME") } else { ("Title", "Album", "Time") };
        format!(
            "     {:>num_w$}  {}{}  {:>time_w$}",
            "#", cell(t, title_w), cell(if album_w > 0 { a } else { "" }, album_w), d,
        )
    };
    let mut lines = vec![tui::line(heading, heading_style(p, focused))];

    let search_active = ui.input_mode.searching() && !ui.library_tree_mode;
    let items_len = crate::ui::shown_len(ui, playlist);
    // A cursor put by a click stays under the pointer (the margin's scroll
    // moved the row, and a double click's second click missed it).
    let visible = ui.cursor >= ui.scroll_offset && ui.cursor < ui.scroll_offset + rows;
    ui.scroll_offset = if ui.list_clicked.take() == Some(ui.cursor) && visible {
        ui.scroll_offset.min(items_len.saturating_sub(rows))
    } else {
        list_scroll(ui.cursor, ui.scroll_offset, items_len, rows)
    };
    let shown = rows.min(items_len.saturating_sub(ui.scroll_offset));
    let row_area = tui::rows(area, 1, rows);
    ui.hits.push((tui::rows(area, 1, shown), Hit::List { first: ui.scroll_offset }));
    let queued = (ui.current + 1)..=(ui.current + ui.enqueue_count);
    if items_len == 0 && search_active {
        lines.push(tui::line("  (no matches)", p.dim));
    }
    // With two panes, the cursor bar shows only in the pane with the keys.
    let show_cursor = focused != Some(false);
    for row in 0..shown {
        let pos = ui.scroll_offset + row;
        let idx = crate::ui::shown_track(ui, pos).unwrap_or(pos);
        let playing = idx == ui.current;
        let cursor = show_cursor && pos == ui.cursor;
        let is_queued = ui.enqueue_count > 0 && queued.contains(&idx);
        let mut title = ui.metadata_cache.title(idx).unwrap_or_else(|| ui.metadata_cache.display_name(idx, &playlist[idx]));
        let artist = ui.metadata_cache.artist_album(idx).0.unwrap_or_default();
        if upper {
            title = title.to_uppercase();
        }
        let album = ui.metadata_cache.album(idx).unwrap_or_default();
        let dur = ui.metadata_cache.duration(idx).map(format_time).unwrap_or_default();
        let marker = if playing { "▶" } else if is_queued { "+" } else { " " };
        let (row_style, mark_style) = if playing { (p.accent, p.accent) } else { (p.fg, p.dim) };
        let mut l = LineBuf::new();
        l.push_str("  ").styled(marker, mark_style).reset().push_str("  ").styled(&format!("{:0>num_w$}", idx + 1), if playing { p.accent } else { p.dim });
        l.reset().push_str("  ");
        // Minimal follows the title with its artist, dim.
        let mut t = LineBuf::new();
        t.styled(&title, row_style);
        if kind == ThemeKind::Minimal && !artist.is_empty() {
            t.reset().push_str("  ").styled(&artist, p.dim);
        }
        l.append(tui::pad(tui::truncate_ellipsis(t.into_line(), title_w.saturating_sub(1)), title_w));
        if album_w > 0 {
            l.styled(&cell(&album, album_w), p.dim);
        }
        l.reset().push_str("  ").styled(&format!("{dur:>time_w$}"), p.dim);
        let l = l.into_line();
        lines.push(if cursor { tui::cursor_row(p, &l, width.saturating_sub(1)) } else { l });
    }
    tui::draw_lines(f.buffer_mut(), area, &lines);
    tui::scrollbar(f, row_area, items_len, ui.scroll_offset, p.accent, p.rule);
}

/// The artist → album tree in `area`: a heading, then the rows.
fn tree_pane(f: &mut Frame, ui: &mut UiState, kind: ThemeKind, p: &Palette, area: Rect, focused: Option<bool>) {
    let rows = (area.height as usize).saturating_sub(1).max(1);
    let heading = match kind {
        ThemeKind::Classic => "  Artist / album",
        ThemeKind::Minimal => "  ARTIST / ALBUM",
        ThemeKind::HiFi => "  ARTISTS",
    };
    let mut lines = vec![tui::line(heading, heading_style(p, focused))];
    // With two panes, the cursor bar shows only in the pane with the keys.
    let body = crate::ui::render_tree_body(ui, rows, (area.width as usize).saturating_sub(1), p, focused != Some(false));
    lines.extend(body);
    let row_area = tui::rows(area, 1, rows);
    ui.hits.push((row_area, Hit::Tree { first: ui.tree_scroll }));
    tui::draw_lines(f.buffer_mut(), area, &lines);
    tui::scrollbar(f, row_area, ui.tree_rows.len(), ui.tree_scroll, p.accent, p.rule);
}

/// The panel's footer: the field being typed, or the keys for the pane that
/// has them, wrapped onto as many rows as `width` needs. (A status message
/// is not in it: it goes in a floating panel's border, `ui::border_status`.)
fn footer(ui: &UiState, kind: ThemeKind, p: &Palette, width: usize, two: bool) -> Vec<FooterRow> {
    let upper = kind == ThemeKind::HiFi;
    let label = |s: &str| if upper { s.to_uppercase() } else { s.to_string() };
    let prompt = |text: &str| {
        let mut l = LineBuf::new();
        l.push_str("  ").styled(text, p.accent).reset().push(' ');
        l.into_line()
    };
    let field = match &ui.input_mode {
        InputMode::Search(q) => Some(crate::ui::field_line(prompt("/"), q, width)),
        InputMode::SavePlaylist(n) => Some(crate::ui::field_line(prompt(&label("save as")), n, width)),
        InputMode::Normal | InputMode::OpenPath(_) => None,
    };
    // With the keys, the field is the footer. Left open while another panel
    // has them, it goes under the keys, whose way in comes first.
    let typing = ui.focus == crate::state::ViewMode::Playlist;
    let field = match field {
        Some((line, col)) if typing => return vec![FooterRow::Field(line, col)],
        other => other,
    };
    let other = if ui.library_tree_mode { "list" } else { "tree" };
    let tab = if two { format!("{other} pane") } else { other.to_string() };
    // The way in and out first: a short panel drops the footer's LAST rows,
    // and those must never be the only way out.
    let mut keys = crate::ui::panel_footer_keys(ui, crate::state::ViewMode::Playlist, upper);
    if ui.library_tree_mode {
        keys.extend([("↵", label("play")), ("←→", label("fold")), ("/", label("filter")), ("d", label("remove")), ("Tab", label(&tab))].map(|(k, l)| (k, l, None)));
    } else {
        keys.extend(
            [
                ("↵", label("play")), ("a", label("queue")), ("/", label("search")), ("d", label("remove")),
                ("s", label("save")), ("⇧s", label("sort")), ("Tab", label(&tab)),
            ]
            .map(|(k, l)| (k, l, None)),
        );
    }
    let mut rows = crate::hints::bar_lines(&keys, &crate::ui::key_look(kind, p), width.saturating_sub(2))
        .into_iter()
        .map(|b| FooterRow::Keys(crate::hints::indent(b, 2)))
        .collect::<Vec<_>>();
    // A search left open goes right after the way in (first row): a short
    // panel drops the footer's last rows, and put last, the line went while
    // its filter still narrowed the list.
    if let Some((line, col)) = field {
        rows.insert(rows.len().min(1), FooterRow::Field(line, col));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_without_the_keys_shows_no_cursor_even_after_it_shrank() {
        let playlist: Vec<std::path::PathBuf> = (0..4).map(|i| format!("/A{i}/t.flac").into()).collect();
        let mut ui = UiState::new(playlist.clone(), crate::metadata::MetadataCache::new(playlist.len()));
        ui.view_mode = crate::state::ViewMode::Playlist; // the tree is built while it shows
        crate::ui::poll_library_tree(&mut ui, &playlist);
        assert!(!ui.tree_rows.is_empty());
        // The tree shrank under its cursor (a filter, a rescan) while the
        // list pane has the keys.
        ui.tree_cursor = 50;
        let p = crate::theme::palette(ThemeKind::Classic);
        let area = Rect::new(0, 0, 40, 12);
        let buf = tui::testing::draw(40, 12, |f| tree_pane(f, &mut ui, ThemeKind::Classic, p, area, Some(false)));
        assert!(ui.tree_cursor < ui.tree_rows.len(), "clamped by the draw");
        let lit = (1..12u16).filter(|&y| (0..40u16).any(|x| buf[(x, y)].bg != ratatui::style::Color::Reset)).count();
        assert_eq!(lit, 0, "a cursor bar in the pane without the keys");
    }

    #[test]
    fn a_search_left_open_shows_right_after_the_way_in() {
        // A short panel drops the footer's LAST rows: the search line, put
        // last, went first while its filter still narrowed the list.
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        ui.docked_shown = vec![crate::state::ViewMode::Playlist];
        ui.focus = crate::state::ViewMode::Player; // the keys are elsewhere
        ui.input_mode = crate::state::InputMode::Search(crate::text_input::TextInput::with("live"));
        let p = crate::theme::palette(ThemeKind::Classic);
        let rows = footer(&ui, ThemeKind::Classic, p, 40, false);
        assert!(rows.len() > 2, "narrow: the keys wrap");
        assert!(matches!(rows[0], FooterRow::Keys(_)), "the way in first");
        assert!(matches!(rows[1], FooterRow::Field(..)), "then the search");
    }

    #[test]
    fn the_way_out_is_on_the_footers_first_row() {
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        ui.docked_shown = vec![crate::state::ViewMode::Playlist];
        ui.focus = crate::state::ViewMode::Playlist;
        let p = crate::theme::palette(ThemeKind::Classic);
        let rows = footer(&ui, ThemeKind::Classic, p, 40, false);
        assert!(rows.len() > 1, "narrow: the keys wrap");
        let FooterRow::Keys(first) = &rows[0] else { panic!("a key row") };
        let text = tui::plain(&first.line);
        for key in ["float", "close", "player"] {
            assert!(text.contains(key), "{key:?} not on the first row: {text:?}");
        }
        // A status message leaves the footer as it is (it goes in the box's
        // border: a row of its own moved the list).
        ui.set_status("playlist docked (k floats it again)".to_string());
        let with = footer(&ui, ThemeKind::Classic, p, 40, false);
        assert_eq!(with.len(), rows.len());
    }
}
