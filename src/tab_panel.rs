//! The tab panel: the playing track's guitar tab (a Guitar Pro file beside
//! it), drawn into a box floating over the player and scrolling with the
//! song on the score's tempo map (`tab_sync::TimeMap`), the user's bar marks
//! stretching it onto the recording.

use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::Frame;
use std::path::PathBuf;

use crate::state::{UiState, ViewMode};
use crate::tab::Score;
use crate::theme::{Palette, ThemeKind};
use crate::tui::{self, LineBuf};
use crate::ui::Ctx;

/// What the panel says when the track has no tab file.
pub const NO_TAB: &str = "No tab: put a Guitar Pro file beside the track, named like it";

/// The panel's title for the box: `Tab` and the file's name.
pub fn title(kind: ThemeKind, ui: &UiState) -> String {
    let mut t = String::from("Tab");
    if let Some(name) = ui.tab_path.as_ref().and_then(|p| p.file_name()) {
        t.push_str(" · ");
        t.push_str(&crate::ansi::sanitize_display(&name.to_string_lossy()));
    }
    if kind == ThemeKind::HiFi { t.to_uppercase() } else { t }
}

/// The parts the panel offers (`↑↓`): the score's tab parts with notes in them.
pub(crate) fn playable_parts(score: &Score) -> Vec<usize> {
    score.tab_parts().into_iter().filter(|&i| !score.parts[i].notes.is_empty()).collect()
}

/// The tempo at `q`: the last change at or before it (120 bpm before any).
fn bpm_at(score: &Score, q: f64) -> f64 {
    let b = score.bar_at(q);
    score.bars[..=b.min(score.bars.len().saturating_sub(1))]
        .iter()
        .rev()
        .find_map(|bar| bar.tempos.iter().rev().find(|t| bar.start_q + t.0 <= q).map(|t| t.1))
        .unwrap_or(120.0)
}

/// The header's items: part · tuning · tempo · time signature · bar ·
/// section, all at the playhead.
fn header_items(score: &Score, part: usize, q_now: f64) -> Vec<String> {
    let mut items = Vec::new();
    if let Some(p) = score.parts.get(part) {
        items.push(p.name.clone());
        items.push(crate::tab::tuning_name(&p.tuning));
    }
    if score.bars.is_empty() {
        return items;
    }
    let b = score.bar_at(q_now);
    items.push(format!("{:.0} bpm", bpm_at(score, q_now)));
    let (num, den) = score.bars[b].time_sig;
    items.push(format!("{num}/{den}"));
    items.push(format!("bar {}/{}", b + 1, score.bars.len()));
    if let Some(s) = score.bars[..=b].iter().rev().find_map(|bar| bar.section.as_ref()) {
        items.push(s.clone());
    }
    items
}

#[cfg(test)]
pub(crate) fn header(score: &Score, part: usize, q_now: f64) -> String {
    header_items(score, part, q_now).join(" · ")
}

/// The bars a tab shows: one behind the playhead, the rest ahead.
const SHOWN_BARS: usize = 4;

/// Cells per quarter note the part needs: 2 for its short notes, so a
/// two-digit fret fits before the next one. One value for the whole part,
/// by its 10th-percentile note (worked out per window, it changed as bars
/// came and went, and the whole tab jumped); the rare shorter note may lose
/// a fret to its neighbour (`put` draws none over another).
fn scale(part: &crate::tab::Part) -> f64 {
    let mut lens: Vec<f64> = part.notes.iter().map(|n| n.len_q).filter(|l| *l > 0.0).collect();
    if lens.is_empty() {
        return 2.0;
    }
    let i = lens.len() / 10;
    let short = *lens.select_nth_unstable_by(i, f64::total_cmp).1;
    (2.0 / short).ceil().clamp(2.0, 16.0)
}

/// Score time `q` to a column of a `w`-cell tab with the playhead at
/// `head`. Everything moves by ONE rounded shift: each item rounded on its
/// own, they stepped on different frames, and bar lines (placed bar by bar
/// from rounded lengths) jumped as the playhead crossed into a new bar.
fn column(q: f64, q_now: f64, cpq: f64, head: usize) -> isize {
    head as isize + (q * cpq).round() as isize - (q_now * cpq).round() as isize
}

/// Each shown bar's line column (a cell before its first note), by bar (the
/// score's last line as bar `bars.len()`), from the bar under the left edge
/// to the first line at or past the right one.
fn place(score: &Score, q_now: f64, cpq: f64, head: usize, w: usize) -> Vec<(usize, isize)> {
    let start = |b: usize| score.bars.get(b).map_or_else(|| score.bars[b - 1].start_q + score.bars[b - 1].len_q, |x| x.start_q);
    let first = score.bar_at((q_now - head as f64 / cpq).max(0.0));
    let mut out = Vec::new();
    for b in first..=score.bars.len() {
        let col = column(start(b), q_now, cpq, head) - 1;
        out.push((b, col));
        if col >= w as isize {
            break;
        }
    }
    out
}

/// The strings' names, highest first (a high E is `e`).
fn string_labels(part: &crate::tab::Part) -> Vec<String> {
    let n = part.tuning.len();
    (0..n)
        .rev()
        .map(|s| {
            let name = crate::tab::note_name(part.tuning[s]);
            if s == n - 1 && name == "E" { "e".into() } else { name }
        })
        .collect()
}

/// The scale of a `w`-cell tab, one for the whole song: about a bar behind and
/// three ahead, the width sharing four of the song's commonest bars (less
/// their lines, in whole cells a bar, or bar lines sat a cell closer here and
/// there), widened only for a part whose notes are too short to fit.
fn cells_per_quarter(score: &Score, part: &crate::tab::Part, w: usize) -> f64 {
    let fill = (w.saturating_sub(SHOWN_BARS + 1) / SHOWN_BARS) as f64 / common_bar_q(score).max(0.25);
    fill.max(scale(part))
}

/// The song's commonest bar length in quarters: the scale is set by it, so a
/// 2/4 bar is drawn half as wide instead of running across twice as fast.
fn common_bar_q(score: &Score) -> f64 {
    let mut counts: Vec<(i64, usize)> = Vec::new();
    for b in &score.bars {
        let k = (b.len_q * 48.0).round() as i64;
        match counts.iter_mut().find(|c| c.0 == k) {
            Some(c) => c.1 += 1,
            None => counts.push((k, 1)),
        }
    }
    counts.iter().max_by_key(|c| c.1).map_or(4.0, |c| c.0 as f64 / 48.0)
}

/// Song seconds at which the tab drawn at `q_now`, `width` columns, next
/// moves a cell, for the loop to wake then: at the frame rate the steps fell
/// on 2 or 3 frames by turns, and the scroll looked uneven.
pub(crate) fn next_step(score: &Score, part: usize, map: &crate::tab_sync::TimeMap, q_now: f64, width: u16) -> Option<f64> {
    let part = score.parts.get(part).filter(|p| !p.tuning.is_empty())?;
    let label_w = string_labels(part).iter().map(|l| tui::width(l)).max().unwrap_or(1) + 1;
    let w = (width as usize).checked_sub(label_w).filter(|&w| w >= 3)?;
    let cpq = cells_per_quarter(score, part, w);
    map.secs(((q_now * cpq).round() + 0.5) / cpq)
}

/// The part's tab around `q_now`, `width` columns: the playhead row (`▼` a
/// bar in, `SHOWN_BARS` across), the strings from the highest down, labelled, a
/// tuplet row, the rhythm row and the lyrics row; `rows` of them at most. The
/// notes sounding at `q_now` are lit in bold accent, the playhead's bar framed
/// in `┃`. The lyrics row has the score's syllables under their beats, else
/// `sung` (the synced lyric line playing now, from Keet's own lyrics). A
/// loop's ends are repeat signs over their bar lines, `‖:` and `:‖`.
#[allow(clippy::too_many_arguments)] // one frame's view of the tab; a struct adds no clarity
pub(crate) fn tab_lines(score: &Score, part: usize, q_now: f64, width: u16, rows: u16, names: bool, sung: Option<&str>, looped: TabLoop, p: &Palette) -> Vec<Line<'static>> {
    let Some(part) = score.parts.get(part) else { return Vec::new() };
    let n = part.tuning.len();
    if score.bars.is_empty() || n == 0 {
        return Vec::new();
    }
    let labels = string_labels(part);
    let label_w = labels.iter().map(|l| tui::width(l)).max().unwrap_or(1) + 1;
    let w = (width as usize).saturating_sub(label_w);
    if w < 3 {
        return Vec::new();
    }
    let head = w / SHOWN_BARS;
    let b0 = score.bar_at(q_now);
    let cpq = cells_per_quarter(score, part, w);
    let lines_at = place(score, q_now, cpq, head, w);
    let first_bar = lines_at[0].0;
    let col_of = |q: f64| column(q, q_now, cpq, head);

    // The grid: the playhead row, the strings (highest first), tuplets, rhythm.
    let (tuplet_row, rhythm_row, lyric_row) = (n + 1, n + 2, n + 3);
    let mut grid: Vec<Vec<(char, Style)>> = (0..n + 4)
        .map(|r| vec![(if (1..=n).contains(&r) { '─' } else { ' ' }, p.dim); w])
        .collect();
    let put = |grid: &mut Vec<Vec<(char, Style)>>, row: usize, col: isize, text: &str, style: Style| {
        // Whole or not at all, and only on free cells (a string's line,
        // a bar line, a blank): a fret cut at the edge, or written over
        // another (12 then 14 in a 64th run read "114"), reads as another.
        let len = text.chars().count() as isize;
        if col < 0 || col + len > w as isize {
            return false;
        }
        let free = |c: &(char, Style)| matches!(c.0, '─' | '│' | '┃' | ' ');
        if !grid[row][col as usize..(col + len) as usize].iter().all(free) {
            return false;
        }
        // On a string, a fret also keeps a free cell from its neighbours
        // (12 beside 14 reads "1214").
        let before = col == 0 || free(&grid[row][col as usize - 1]);
        let after = grid[row].get((col + len) as usize).is_none_or(free);
        if (1..=n).contains(&row) && !(before && after) {
            return false;
        }
        for (i, c) in text.chars().enumerate() {
            grid[row][col as usize + i] = (c, style);
        }
        true
    };
    put(&mut grid, 0, head as isize, "▼", p.accent);
    let (from, to) = match looped {
        TabLoop::Off => (None, None),
        TabLoop::From(a) => (Some(a), None),
        TabLoop::On { from, to, .. } => (Some(from), Some(to + 1)),
    };
    for &(b, col) in &lines_at {
        if Some(b) == from {
            put(&mut grid, 0, col, "‖:", p.accent);
        }
        if Some(b) == to {
            put(&mut grid, 0, col - 1, ":‖", p.accent);
        }
    }
    for &(b, col) in &lines_at {
        let framed = b == b0 || b == b0 + 1;
        let (glyph, style) = if framed { ("┃", p.accent) } else { ("│", p.dim) };
        for row in 1..=n {
            put(&mut grid, row, col, glyph, style);
        }
    }
    let from = score.bars[first_bar].start_q;
    let last_shown = lines_at.last().map_or(b0, |l| l.0.min(score.bars.len() - 1));
    let to = score.bars[last_shown].start_q + score.bars[last_shown].len_q;
    let lo = part.notes.partition_point(|x| x.q < from);
    let notes = &part.notes[lo..part.notes.partition_point(|x| x.q < to)];
    let mut onset = f64::NAN;
    let mut drawn = Vec::new();
    for (i, note) in notes.iter().enumerate() {
        let col = col_of(note.q);
        // A tie sounds on, not played again: shown only where it crosses a bar line.
        if note.tied && (score.bars[score.bar_at(note.q)].start_q - note.q).abs() > 1e-6 {
            continue;
        }
        let fret = if names {
            crate::tab::note_name(part.tuning[note.string as usize].saturating_add(note.fret))
        } else {
            note.fret.to_string()
        };
        let text = match () {
            _ if note.tied => format!("({fret})"),
            _ if note.fx.dead => "x".to_string(),
            _ if note.fx.harmonic => format!("<{fret}>"),
            _ => fret,
        };
        let sounding = note.q <= q_now && q_now < note.q + note.len_q;
        let style = if sounding { p.accent.patch(p.bold) } else { p.fg };
        let row = n - note.string as usize;
        if put(&mut grid, row, col, &text, style) {
            drawn.push((lo + i, row, col, text.chars().count() as isize));
        }
        if note.tied {
            continue;
        }
        if note.q != onset {
            onset = note.q;
            let base = note.len_q / (2.0 - 0.5f64.powi(note.dots.min(8) as i32))
                * note.tuplet.map_or(1.0, |(num, den)| num as f64 / den as f64);
            let glyph = match base {
                b if b >= 4.0 - 1e-9 => "o",
                b if b >= 2.0 - 1e-9 => "d",
                b if b >= 1.0 - 1e-9 => "♩",
                b if b >= 0.5 - 1e-9 => "♪",
                _ => "♬",
            };
            let dot = if note.dots > 0 { "." } else { "" };
            put(&mut grid, rhythm_row, col, &format!("{glyph}{dot}"), p.dim);
            if let Some((num, _)) = note.tuplet {
                put(&mut grid, tuplet_row, col, &num.to_string(), p.dim);
            }
        }
    }
    // Techniques: one dim character a cell, round the fret, on a free string
    // cell only (never a bar line, never over another fret).
    let mark = |grid: &mut Vec<Vec<(char, Style)>>, row: usize, col: isize, c: char| {
        let cell = usize::try_from(col).ok().and_then(|col| grid[row].get_mut(col)).filter(|cell| cell.0 == '─');
        cell.map(|cell| *cell = (c, p.dim)).is_some()
    };
    for &(i, row, col, len) in &drawn {
        let note = &part.notes[i];
        let fx = note.fx;
        let up = part.notes[i + 1..].iter().find(|m| m.string == note.string && !m.tied).map(|m| m.fret > note.fret);
        let mut after = String::new();
        if fx.bend {
            after.push('b');
            if fx.release {
                after.push('r');
            }
        }
        match up {
            Some(up) if fx.slide & 3 != 0 => after.push(if up { '/' } else { '\\' }),
            Some(up) if fx.hopo => after.push(if up { 'h' } else { 'p' }),
            _ => {}
        }
        if fx.slide & 4 != 0 {
            after.push('\\');
        } else if fx.slide & 8 != 0 {
            after.push('/');
        }
        if fx.vibrato {
            after.push('~');
        }
        for (k, c) in after.chars().enumerate() {
            if !mark(&mut grid, row, col + len + k as isize, c) {
                break;
            }
        }
        if fx.slide & 16 != 0 {
            mark(&mut grid, row, col - 1, '/');
        } else if fx.slide & 32 != 0 {
            mark(&mut grid, row, col - 1, '\\');
        }
    }
    // Palm mute and let ring: named under the strings where a run of them starts.
    let mut open = [false; 2];
    for group in notes.chunk_by(|a, b| a.q == b.q) {
        let col = col_of(group[0].q);
        for (k, (on, labels)) in [(group.iter().any(|m| m.fx.palm_mute), &["PM"][..]), (group.iter().any(|m| m.fx.let_ring), &["let ring", "ring"][..])].into_iter().enumerate() {
            open[k] = on && (open[k] || labels.iter().any(|l| put(&mut grid, tuplet_row, col, l, p.dim)));
        }
    }

    // Rests: `-` where a gap in the part starts, in every bar shown.
    for &(b, _) in lines_at.iter().filter(|l| l.0 < score.bars.len()) {
        let bar = &score.bars[b];
        let end = bar.start_q + bar.len_q;
        let mut t = bar.start_q;
        let in_bar = &part.notes[part.notes.partition_point(|x| x.q < bar.start_q)..part.notes.partition_point(|x| x.q < end)];
        let rest_at = |grid: &mut Vec<Vec<(char, Style)>>, q: f64| {
            let col = col_of(q);
            if (0..w as isize).contains(&col) && grid[rhythm_row][col as usize].0 == ' ' {
                put(grid, rhythm_row, col, "-", p.dim);
            }
        };
        for note in in_bar {
            if note.q > t + 1e-6 {
                rest_at(&mut grid, t);
            }
            t = t.max(note.q + note.len_q);
        }
        if end > t + 1e-6 {
            rest_at(&mut grid, t);
        }
    }

    // Lyrics: each syllable at its beat, a blank after it so words never run
    // together (a syllable with no room is left out); else the line sung now.
    let sung = sung.filter(|_| score.lyrics.is_empty());
    if !score.lyrics.is_empty() {
        let lyrics = &score.lyrics[score.lyrics.partition_point(|l| l.0 < from)..score.lyrics.partition_point(|l| l.0 < to)];
        for (q, word) in lyrics {
            let sounding = *q <= q_now && lyrics.iter().find(|l| l.0 > *q).is_none_or(|l| q_now < l.0);
            let style = if sounding { p.accent.patch(p.bold) } else { p.fg };
            let col = col_of(*q);
            // shortcut: one char a cell, so a syllable with a wide or combining character
            // (CJK, emoji) is left out; measure by width if such a tab turns up.
            if !word.chars().all(crate::ansi::single_cell) {
                continue;
            }
            if put(&mut grid, lyric_row, col, word, style) {
                if let Some(cell) = grid[lyric_row].get_mut((col + word.chars().count() as isize) as usize) {
                    *cell = ('\u{a0}', p.fg);
                }
            }
        }
    }

    grid.into_iter()
        .enumerate()
        .take(rows as usize)
        .map(|(r, cells)| {
            if let Some(text) = sung.filter(|_| r == lyric_row) {
                let mut l = LineBuf::new();
                l.reset().spaces(label_w);
                let mut line = l.into_line();
                line.spans.extend(tui::truncate(tui::line(text.to_string(), p.fg), w).spans);
                return line;
            }
            let mut l = LineBuf::new();
            match labels.get(r.wrapping_sub(1)).filter(|_| (1..=n).contains(&r)) {
                Some(label) => l.styled(label, p.fg).spaces(label_w - tui::width(label)),
                None => l.reset().spaces(label_w),
            };
            for (c, style) in cells {
                l.style(style).push(c);
            }
            l.into_line()
        })
        .collect()
}

/// Score time to song time again, after the score or the sync changed.
/// With the track's onsets known, the beat tracker's corrections are worked
/// out again from the marks (without the nudge, which goes on top).
/// The tracker runs only when its input changed (the marks, the onsets, a
/// new load), not for a nudge or a part (Ruling 15).
fn remap(ui: &mut UiState) {
    use crate::tab_sync::{track_beats, TimeMap, TrackSync};
    let Some(s) = ui.tab.clone() else {
        ui.tab_map = None;
        return;
    };
    let onsets = ui.tab_onsets.as_ref().filter(|o| Some(&o.0) == ui.tab_audio.as_ref());
    let key = (ui.tab_gen, ui.tab_sync.marks.clone(), onsets.is_some());
    if ui.tab_corrections.as_ref().is_none_or(|c| c.0 != key) {
        let corrections = onsets.map_or_else(Vec::new, |o| {
            let marks_only = TrackSync { nudge: 0.0, ..ui.tab_sync.clone() };
            track_beats(&TimeMap::new(&s, &marks_only, &[]), &s, &o.1)
        });
        ui.tab_corrections = Some((key, corrections));
    }
    let corrections = ui.tab_corrections.as_ref().map_or(&[][..], |c| &c.1[..]);
    ui.tab_map = Some(TimeMap::new(&s, &ui.tab_sync, corrections));
}

/// Take the beat worker's onset curve, if it came and is for this load.
fn take_beats(ui: &mut UiState) -> bool {
    let Some(rx) = &ui.tab_beat_receiver else { return false };
    let answer = match rx.try_recv() {
        Err(std::sync::mpsc::TryRecvError::Empty) => return false,
        Err(_) => None,
        Ok((gen, _)) if gen != ui.tab_gen => return false,
        Ok((_, answer)) => answer.ok(),
    };
    ui.tab_beat_receiver = None;
    // Unreadable audio: the tab runs on the tempo map and the marks.
    let (Some(onsets), Some(audio)) = (answer, ui.tab_audio.clone()) else { return false };
    ui.tab_onsets = Some((audio, onsets));
    remap(ui);
    true
}

/// Keep the sync for the track (and the time map with it).
fn save(ui: &mut UiState) {
    if let Some(audio) = ui.tab_audio.clone() {
        ui.tab_syncs.set(&audio, &ui.tab_sync);
    }
    remap(ui);
}

/// Take the worker's answer, if it came and is for the current load.
/// Returns whether anything changed on screen.
pub(crate) fn take_answer(ui: &mut UiState) -> bool {
    use std::sync::mpsc::TryRecvError;
    let Some(rx) = &ui.tab_receiver else { return false };
    let answer = match rx.try_recv() {
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => {
            ui.tab_receiver = None;
            return true;
        }
        // An older load's (the last track's): this one's is still to come.
        Ok((gen, _)) if gen != ui.tab_gen => return false,
        Ok((_, answer)) => answer,
    };
    ui.tab_receiver = None;
    ui.tab_loop = TabLoop::Off;
    match answer {
        Ok((file, score)) => {
            let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let audio = ui.tab_audio.clone().unwrap_or_default();
            ui.tab_cleared = None;
            ui.tab_sync = ui.tab_syncs.get(&audio, &name);
            ui.tab_sync.tab_file = name;
            let parts = playable_parts(&score);
            if !parts.contains(&ui.tab_sync.part) {
                ui.tab_sync.part = parts.first().copied().unwrap_or(0);
            }
            ui.tab = Some(std::sync::Arc::new(score));
            ui.tab_path = Some(file);
            ui.tab_error = None;
            remap(ui);
            // Decoded once per track: a reopen or a reread tab reuses it.
            if ui.tab_audio.is_some() && ui.tab_onsets.as_ref().map(|o| &o.0) != Some(&audio) {
                crate::spawn_beat_worker(ui, audio);
            }
        }
        Err(why) => {
            ui.tab = None;
            ui.tab_map = None;
            ui.tab_path = None;
            // No file: the panel says where it looks.
            if why.is_empty() {
                return true;
            }
            let why = crate::ansi::sanitize_display(&why);
            ui.set_status(format!("tab: {why}"));
            ui.tab_error = Some(why);
        }
    }
    true
}

/// Whether the tab panel is up: docked and shown in the last frame (hidden
/// for want of room, behind HiFi's lyrics or in full window, it runs
/// nothing), floating, or under the key list or the EQ.
fn up(ui: &UiState) -> bool {
    ui.docked_shown.contains(&ViewMode::Tab) || ui.view_mode == ViewMode::Tab || crate::ui::base_view(ui) == ViewMode::Tab
}

/// The track to look for a tab for: the playing one, while the panel is up
/// and its tab was looked for another track (or none yet). A closed panel
/// reads no files.
pub(crate) fn wanted(ui: &UiState, playlist: &[PathBuf]) -> Option<PathBuf> {
    let path = playlist.get(ui.current)?;
    (up(ui) && ui.tab_audio.as_ref() != Some(path)).then(|| path.clone())
}

/// The loop's part, every pass: start a load the panel wants, take an
/// answer. Returns whether anything changed on screen.
pub(crate) fn step(ui: &mut UiState, playlist: &[PathBuf]) -> bool {
    // Closed with nothing found (or unreadable): the next open looks again,
    // for a file put there meanwhile.
    if !up(ui) && ui.tab.is_none() && ui.tab_receiver.is_none() {
        ui.tab_audio = None;
    }
    // A closed panel keeps nothing in memory; opening loads again.
    if !up(ui) && ui.tab_receiver.is_none() && (ui.tab.is_some() || ui.tab_onsets.is_some()) {
        ui.tab = None;
        ui.tab_onsets = None;
        ui.tab_map = None;
        ui.tab_corrections = None;
        ui.tab_cleared = None;
        ui.tab_audio = None;
        ui.tab_loop = TabLoop::Off;
    }
    let mut changed = false;
    if let Some(path) = wanted(ui, playlist) {
        let (artist, title) = ui.metadata_cache.artist_title(ui.current);
        crate::spawn_tab_worker(ui, path, title, artist);
        changed = true;
    }
    let beats = take_beats(ui);
    take_answer(ui) || beats || changed
}

/// A shown tab that moves with the song (bar 1 marked): it needs the
/// visualization's frame rate while playing.
pub(crate) fn moving(ui: &UiState) -> bool {
    up(ui) && ui.tab.is_some() && !ui.tab_sync.marks.is_empty()
}

/// A loop over bars (`r`): the first press marks the bar it starts in, the
/// second the bar it ends in (the same bar loops that bar), a third lets go.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum TabLoop {
    #[default]
    Off,
    From(usize),
    /// Bars `from..=to`. `armed` once playback is seen inside: until a seek
    /// back lands, the clock still reads past the end.
    On { from: usize, to: usize, armed: bool },
}

/// How far outside the loop a seek must land to let go of it, not count as
/// a seek landing early (FLAC seeks are coarse) or a late wake.
const LOOP_LEAVE: f64 = 0.5;

/// The loop's start and end in song seconds.
fn loop_span(ui: &UiState) -> Option<(f64, f64)> {
    let TabLoop::On { from, to, .. } = ui.tab_loop else { return None };
    let (s, map) = (ui.tab.as_ref()?, ui.tab_map.as_ref()?);
    let end = s.bars.get(to).map(|b| b.start_q + b.len_q)?;
    Some((map.secs(s.bars.get(from)?.start_q)?, map.secs(end)?))
}

/// Song seconds at which a loop playing now ends (the loop wakes then).
pub(crate) fn loop_end(ui: &UiState) -> Option<f64> {
    matches!(ui.tab_loop, TabLoop::On { armed: true, .. }).then(|| loop_span(ui)).flatten().map(|s| s.1)
}

/// The loop's part, every pass, at song time `now`: the seek (seconds from
/// now) back to the loop's start once playback reaches its end. A seek that
/// lands outside the loop lets go of it; one inside plays on.
pub(crate) fn loop_seek(ui: &mut UiState, now: f64) -> Option<f64> {
    let TabLoop::On { from, to, armed } = ui.tab_loop else { return None };
    let (start, end) = loop_span(ui)?;
    if !armed {
        if now < end {
            ui.tab_loop = TabLoop::On { from, to, armed: true };
        }
        return None;
    }
    if now < start - LOOP_LEAVE || now > end + LOOP_LEAVE {
        ui.tab_loop = TabLoop::Off;
        return None;
    }
    if now < end {
        return None;
    }
    ui.tab_loop = TabLoop::On { from, to, armed: false };
    Some(start - now)
}

/// The panel's own keys (Esc and `g` are the caller's); false for a key it
/// leaves to the player.
pub(crate) fn handle_key(ui: &mut UiState, now: f64, code: KeyCode) -> bool {
    if code == KeyCode::Char('n') {
        ui.tab_names = !ui.tab_names;
        return true;
    }
    let Some(score) = ui.tab.clone() else { return false };
    let step = |x: f64, d: f64| ((x + d) * 100.0).round() / 100.0;
    match code {
        // Up/Down, not ←→: those stay the player's seek (a song skip matters less here).
        KeyCode::Up | KeyCode::Down => {
            let parts = playable_parts(&score);
            if parts.is_empty() {
                return true;
            }
            let i = parts.iter().position(|&p| p == ui.tab_sync.part).unwrap_or(0);
            let next = if code == KeyCode::Down { (i + 1) % parts.len() } else { (i + parts.len() - 1) % parts.len() };
            ui.tab_sync.part = parts[next];
        }
        KeyCode::Enter => {
            if let Some(map) = ui.tab_map.as_ref() {
                crate::tab_sync::mark_now(&mut ui.tab_sync, map, &score, now);
                ui.tab_cleared = None;
            }
        }
        KeyCode::Backspace => match ui.tab_cleared.take() {
            Some(old) => ui.tab_sync = old,
            None => crate::tab_sync::undo_mark(&mut ui.tab_sync),
        },
        KeyCode::Char(c @ (',' | '.')) => {
            if !crate::tab_sync::move_mark(&mut ui.tab_sync, &score, if c == ',' { -1 } else { 1 }) {
                return true;
            }
        }
        KeyCode::Char('r') => {
            let Some(q) = ui.tab_map.as_ref().filter(|m| m.secs(0.0).is_some()).map(|m| m.q_at(now)) else {
                ui.set_status("mark bar 1 first (Enter), then r loops".into());
                return true;
            };
            let bar = score.bar_at(q);
            ui.tab_loop = match ui.tab_loop {
                TabLoop::Off => TabLoop::From(bar),
                TabLoop::From(a) => TabLoop::On { from: a.min(bar), to: a.max(bar), armed: false },
                TabLoop::On { .. } => TabLoop::Off,
            };
            // Not the track's sync: nothing to save.
            return true;
        }
        KeyCode::Char('a') => ui.tab_sync.nudge = step(ui.tab_sync.nudge, -0.05),
        KeyCode::Char('d') => ui.tab_sync.nudge = step(ui.tab_sync.nudge, 0.05),
        KeyCode::Char('0') => {
            if !ui.tab_sync.marks.is_empty() || ui.tab_sync.nudge != 0.0 {
                ui.tab_cleared = Some(ui.tab_sync.clone());
            }
            ui.tab_sync.marks.clear();
            ui.tab_sync.nudge = 0.0;
        }
        _ => return false,
    }
    save(ui);
    true
}

/// Rows the floating box wants: the borders, the header and a blank row,
/// the tab (the playhead row, the strings, tuplets, rhythm, lyrics) and two footer rows.
pub(crate) fn box_rows(ui: &UiState) -> u16 {
    let strings = ui.tab.as_ref().and_then(|s| s.parts.get(ui.tab_sync.part)).map_or(6, |p| p.tuning.len());
    rows_for(strings as u16)
}

/// The box's rows for a part of `strings` strings (`box_rows`).
pub(crate) const fn rows_for(strings: u16) -> u16 {
    2 + 2 + (strings + 4) + 2
}

/// Draw the panel inside `area`: the header, the tab, then the footer.
pub fn draw(f: &mut Frame, cx: &mut Ctx, area: Rect) {
    let kind = cx.state.theme_kind();
    let p = crate::theme::palette(kind);
    if area.height < 2 || area.width < 4 {
        return;
    }
    let ui = &mut *cx.ui;
    let width = area.width as usize;
    let upper = kind == ThemeKind::HiFi;
    let label = |s: &str| if upper { s.to_uppercase() } else { s.to_string() };
    let score = ui.tab.clone();
    let marked = !ui.tab_sync.marks.is_empty();
    let parts = score.as_deref().map(playable_parts).unwrap_or_default();
    // The way in and out first: a short panel drops the footer's last rows.
    let mut keys = crate::ui::panel_footer_keys(ui, ViewMode::Tab, upper);
    if score.is_some() && !marked {
        keys.push(("↵", label("when bar 1 starts"), None));
    }
    if score.is_some() {
        if parts.len() > 1 {
            keys.push(("↑↓", label("part"), None));
        }
        if marked {
            keys.push(("↵", label("bar starts now"), None));
            keys.push(("⌫", label("undo"), None));
            keys.push((",/.", label("mark a bar earlier/later"), None));
        }
        // The unit stays lower case in HiFi's capitals.
        keys.push(("a/d", label("nudge ±0.05") + " s", None));
        if marked || ui.tab_sync.nudge != 0.0 {
            keys.push(("0", label("clear"), None));
        }
        keys.push(("n", label(if ui.tab_names { "frets" } else { "notes" }), None));
        if marked {
            let looping = match ui.tab_loop {
                TabLoop::Off => "loop from this bar".to_string(),
                TabLoop::From(a) => format!("loop bar {} to this bar", a + 1),
                TabLoop::On { from, to, .. } if from == to => format!("stop looping bar {}", from + 1),
                TabLoop::On { from, to, .. } => format!("stop looping bars {}–{}", from + 1, to + 1),
            };
            keys.push(("r", label(&looping), None));
        }
    }
    let look = crate::ui::key_look(kind, p);
    let footer: Vec<crate::hints::Bar> =
        crate::hints::bar_lines(&keys, &look, width.saturating_sub(2)).into_iter().map(|b| crate::hints::indent(b, 2)).collect();
    let footer_h = (footer.len() as u16).min(area.height - 1);
    let body = Rect { height: area.height - footer_h, ..area };
    let say = |what: &str| -> Vec<Line<'static>> {
        let mut out = vec![Line::default()];
        out.extend(tui::wrap(&label(what), width.saturating_sub(4)).into_iter().map(|r| tui::line(format!("  {r}"), p.dim)));
        out
    };
    let lines = match score.as_deref() {
        None if ui.tab_receiver.is_some() => say("looking for a tab…"),
        None => match ui.tab_error.clone() {
            Some(why) => say(&format!("can't read the tab: {why}")),
            None => say(NO_TAB),
        },
        Some(_) if parts.is_empty() => say("no guitar or bass part with notes in this tab"),
        Some(s) => {
            let q = ui.tab_map.as_ref().map_or(0.0, |m| m.q_at(cx.state.time_secs()));
            let part = ui.tab_sync.part;
            let items: Vec<Line<'static>> = header_items(s, part, q)
                .into_iter()
                .enumerate()
                .map(|(i, t)| tui::line(label(&t), if i == 0 { p.accent.patch(p.bold) } else { p.fg }))
                .collect();
            let sep = tui::line(" · ", p.dim);
            let mut out = vec![tui::fit_segments(&items, &sep, width), Line::default()];
            let clock = cx.state.time_secs() + ui.lyrics_offset;
            let sung = ui.lyrics().filter(|l| l.is_synced()).and_then(|l| l.current_line(clock).map(|i| l.line_text(i).to_string()));
            out.extend(tab_lines(s, part, q, area.width, body.height.saturating_sub(2), ui.tab_names, sung.as_deref(), ui.tab_loop, p));
            ui.tab_next_step = ui.tab_map.as_ref().and_then(|m| next_step(s, part, m, q, area.width));
            out
        }
    };
    tui::draw_lines(f.buffer_mut(), body, &lines);
    for (i, bar) in footer.iter().take(footer_h as usize).enumerate() {
        let y = body.bottom() + i as u16;
        bar.register(&mut ui.hits, area.x, y);
        tui::draw_lines(f.buffer_mut(), Rect { y, height: 1, ..area }, std::slice::from_ref(&bar.line));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tab::{Bar, Note, Part, PartKind};

    fn note(q: f64, len_q: f64, string: u8, fret: u8) -> Note {
        Note { q, len_q, string, fret, tied: false, dots: 0, tuplet: None, fx: Default::default() }
    }

    /// `bars` bars of 4/4 at 85 bpm, "Intro" on the first; a Lead Guitar in
    /// E standard (a quarter on low E fret 3, a dotted eighth fret 12, then a
    /// rest), an empty part, and a Rhythm Guitar.
    pub(crate) fn score(bars: usize) -> Score {
        let mut dotted = note(1.0, 0.75, 0, 12);
        dotted.dots = 1;
        Score {
            title: "Song".into(),
            artist: "Band".into(),
            bars: (0..bars)
                .map(|i| Bar {
                    start_q: 4.0 * i as f64,
                    len_q: 4.0,
                    time_sig: (4, 4),
                    tempos: if i == 0 { vec![(0.0, 85.0)] } else { vec![] },
                    section: (i == 0).then(|| "Intro".to_string()),
                })
                .collect(),
            parts: vec![
                Part { name: "Lead Guitar".into(), kind: PartKind::Guitar, tuning: vec![40, 45, 50, 55, 59, 64], notes: vec![note(0.0, 1.0, 0, 3), dotted] },
                Part { name: "Empty".into(), kind: PartKind::Guitar, tuning: vec![40, 45, 50, 55, 59, 64], notes: vec![] },
                Part { name: "Rhythm Guitar".into(), kind: PartKind::Guitar, tuning: vec![38, 45, 50, 55, 59, 64], notes: vec![note(0.0, 4.0, 0, 0)] },
            ],
            lyrics: vec![],
        }
    }

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The column where `pat` starts in `s`.
    fn col(s: &str, pat: &str) -> Option<usize> {
        s.find(pat).map(|i| crate::tui::width(&s[..i]))
    }

    fn lines(q_now: f64, width: u16, names: bool) -> Vec<String> {
        let p = crate::theme::palette(ThemeKind::Classic);
        let l = tab_lines(&score(3), 0, q_now, width, 20, names, None, TabLoop::Off, p);
        for line in &l {
            assert!(crate::tui::width(&text(line)) <= width as usize, "{:?} wider than {width}", text(line));
        }
        l.iter().map(text).collect()
    }

    #[test]
    fn the_tab_reads_from_the_high_string_down_and_scrolls_past_the_playhead() {
        let rows = lines(0.0, 80, false);
        let head = col(&rows[0], "▼").expect("the playhead");
        assert!((18..=21).contains(&head), "a bar in, four bars across: {head}");
        assert!(rows[1].starts_with('e'), "high e on top: {:?}", rows[1]);
        let low = &rows[6];
        assert!(low.starts_with('E'), "low E last: {low:?}");
        assert!(col(low, "3").unwrap() >= head, "ahead of (or under) the playhead: {low:?}");
        let later = lines(3.0, 80, false);
        assert!(col(&later[6], "3").is_none_or(|c| c < head), "scrolled left past it: {:?}", later[6]);
        // The rhythm row: ♩ under the quarter, ♪. under the dotted eighth, - for the rest.
        let rhythm = &rows[8];
        assert_eq!(col(rhythm, "♩"), col(low, "3"), "{rhythm:?}");
        assert_eq!(col(rhythm, "♪."), col(low, "12"), "{rhythm:?}");
        assert!(col(rhythm, "-").unwrap() > col(rhythm, "♪.").unwrap(), "{rhythm:?}");
        // Note names: G on low E's third fret.
        let named = lines(0.0, 80, true);
        assert!(named[6].contains('G') && !named[6].contains('3'), "{:?}", named[6]);
    }

    #[test]
    fn the_tab_shows_a_bar_behind_and_three_ahead_at_one_scale_through_the_bar() {
        let p = crate::theme::palette(ThemeKind::Classic);
        let bar_lines = |q: f64| -> Vec<usize> {
            let l = tab_lines(&score(8), 0, q, 80, 20, false, None, TabLoop::Off, p);
            text(&l[1]).chars().enumerate().filter(|(_, c)| matches!(c, '│' | '┃')).map(|(i, _)| i).collect()
        };
        let gaps = |v: &[usize]| v.windows(2).map(|w| w[1] - w[0]).collect::<Vec<_>>();
        let (early, late) = (bar_lines(8.0), bar_lines(11.9));
        assert!(gaps(&early).iter().all(|&g| (18..=20).contains(&g)), "a quarter of the width a bar: {early:?}");
        assert!(gaps(&late).iter().all(|&g| g == gaps(&early)[0]), "the scale holds through the bar: {early:?} {late:?}");
        let head = 2 + 78 / 4;
        assert_eq!(early.iter().filter(|&&c| c > head).count(), 3, "three bar lines ahead: {early:?}");
    }

    #[test]
    fn a_bar_line_steps_left_one_cell_at_a_time_across_a_bar_change() {
        let s = score(8);
        let mut last = None;
        for i in 0..=200 {
            let q = 3.0 + i as f64 * 0.01;
            let col = place(&s, q, 4.5, 19, 78).into_iter().find(|l| l.0 == 2).map(|l| l.1).unwrap();
            if let Some(prev) = last {
                assert!(prev - col == 0 || prev - col == 1, "q {q}: {prev} -> {col}");
            }
            last = Some(col);
        }
    }

    #[test]
    fn a_short_bar_is_drawn_narrower_not_faster() {
        let four = score(8);
        let mut s = score(8);
        s.bars[3].len_q = 2.0;
        s.bars[3].time_sig = (2, 4);
        for b in &mut s.bars[4..] {
            b.start_q -= 2.0;
        }
        let cpq = cells_per_quarter(&s, &s.parts[0], 78);
        assert_eq!(cpq, cells_per_quarter(&four, &four.parts[0], 78));
        let at = |q| place(&s, q, cpq, 19, 78);
        let line = |b: usize| at(13.0).into_iter().find(|l| l.0 == b).unwrap().1;
        assert_eq!(line(5) - line(4), (line(4) - line(3)) * 2, "the 2/4 bar half as wide");
    }

    #[test]
    fn the_loop_wakes_when_the_tab_moves_a_cell() {
        let s = score(8);
        let sync = crate::tab_sync::TrackSync { marks: vec![(0, 1.0)], ..Default::default() };
        let map = crate::tab_sync::TimeMap::new(&s, &sync, &[]);
        let cpq = cells_per_quarter(&s, &s.parts[0], 78);
        for q in [0.0, 1.3, 3.99] {
            let t = next_step(&s, 0, &map, q, 80).unwrap();
            let at = |t: f64| column(0.0, map.q_at(t), cpq, 19);
            assert!(t > map.secs(q).unwrap(), "ahead of now");
            assert_eq!(at(t - 1e-4), column(0.0, q, cpq, 19), "still before it, q {q}");
            assert_eq!(at(t + 1e-4), column(0.0, q, cpq, 19) - 1, "a cell on at it, q {q}");
        }
    }

    #[test]
    fn techniques_are_drawn_round_the_fret() {
        use crate::tab::Effects;
        let fx = |f: fn(&mut Effects)| {
            let mut e = Effects::default();
            f(&mut e);
            e
        };
        let with = |q, string, fret, fx: Effects| Note { fx, ..note(q, 1.0, string, fret) };
        let mut notes = vec![
            with(0.0, 0, 5, fx(|e| e.hopo = true)),
            with(1.0, 0, 7, fx(|e| e.hopo = true)),
            note(2.0, 1.0, 0, 5),
            with(0.0, 1, 7, fx(|e| e.slide = 1)),
            with(1.0, 1, 9, fx(|e| e.slide = 4)),
            with(0.0, 2, 7, fx(|e| (e.bend, e.release) = (true, true))),
            with(1.0, 2, 9, fx(|e| e.vibrato = true)),
            with(1.0, 3, 0, fx(|e| e.dead = true)),
            with(2.0, 3, 12, fx(|e| e.harmonic = true)),
            with(1.0, 4, 5, fx(|e| e.slide = 16)),
            with(0.0, 5, 1, fx(|e| e.palm_mute = true)),
            with(1.0, 5, 1, fx(|e| e.palm_mute = true)),
            note(2.0, 1.0, 5, 3),
            with(3.0, 5, 3, fx(|e| e.let_ring = true)),
            Note { tied: true, ..note(4.0, 1.0, 5, 3) },
            Note { tied: true, ..note(4.5, 0.5, 5, 3) },
        ];
        notes.sort_by(|a, b| a.q.total_cmp(&b.q));
        let mut s = score(3);
        s.parts[0].notes = notes;
        let rows: Vec<String> = tab_lines(&s, 0, 0.0, 80, 20, false, None, TabLoop::Off, crate::theme::palette(ThemeKind::Classic)).iter().map(text).collect();
        let has = |r: usize, pat: &str| assert!(rows[r].contains(pat), "{pat:?} in {:?}", rows[r]);
        has(6, "5h");
        has(6, "7p");
        has(5, "7/");
        has(5, "9\\");
        has(4, "7br");
        has(4, "9~");
        has(3, "x");
        has(3, "<12>");
        has(2, "/5");
        has(1, "(3)");
        assert_eq!(rows[1].matches("(3)").count(), 1, "a tie inside a bar is not drawn");
        has(7, "PM");
        assert_eq!(rows[7].matches("PM").count(), 1, "once a run");
        has(7, "let ring");
    }

    #[test]
    fn the_scores_lyrics_sit_under_their_notes_else_the_line_sung_now() {
        let p = crate::theme::palette(ThemeKind::Classic);
        let mut s = score(3);
        s.parts[0].notes = vec![note(0.0, 0.25, 0, 3), note(0.25, 0.25, 0, 5), note(1.0, 1.0, 0, 7)];
        s.lyrics = vec![(0.0, "Hel-".into()), (0.25, "lo".into()), (1.0, "world".into())];
        let rows: Vec<String> = tab_lines(&s, 0, 0.0, 80, 20, false, Some("not this"), TabLoop::Off, p).iter().map(text).collect();
        let (low, sung) = (&rows[6], &rows[9]);
        assert_eq!(col(sung, "Hel-"), col(low, "3"), "under its note: {sung:?}");
        assert_eq!(col(sung, "world"), col(low, "7"));
        assert!(!sung.contains("lo"), "no room before the next beat: left out, never run together: {sung:?}");
        assert!(!sung.contains("not this"), "the score's lyrics win");
        s.lyrics.clear();
        let rows: Vec<String> = tab_lines(&s, 0, 0.0, 80, 20, false, Some("the line sung now"), TabLoop::Off, p).iter().map(text).collect();
        assert!(rows[9].trim_start().starts_with("the line sung now"), "{:?}", rows[9]);
    }

    #[test]
    fn a_two_digit_fret_is_never_split() {
        for width in [12u16, 20, 31, 40, 80] {
            for step in 0..64 {
                let low = &lines(step as f64 * 0.125, width, false)[6];
                let digits: String = low.chars().filter(|c| *c == '1' || *c == '2').collect();
                assert!(digits.is_empty() || digits == "12", "width {width}, q {}: {low:?}", step as f64 * 0.125);
            }
        }
    }

    #[test]
    fn the_sounding_note_is_lit_and_bold_and_the_playheads_bar_is_framed() {
        let p = crate::theme::palette(ThemeKind::Classic);
        let l = tab_lines(&score(3), 0, 0.5, 80, 20, false, None, TabLoop::Off, p);
        let span = |pat: &str| l[6].spans.iter().find(|s| s.content.contains(pat)).cloned().expect(pat);
        let lit = span("3");
        assert!(lit.style.add_modifier.contains(ratatui::style::Modifier::BOLD), "{lit:?}");
        assert_eq!(lit.style.fg, p.accent.fg, "{lit:?}");
        assert!(!span("12").style.add_modifier.contains(ratatui::style::Modifier::BOLD), "not sounding yet");
        let low = text(&l[6]);
        assert_eq!(low.matches('┃').count(), 2, "the playhead's bar framed: {low:?}");
        assert!(low.contains('│'), "the other bars plain: {low:?}");
    }

    #[test]
    fn the_header_names_part_tuning_tempo_and_bar() {
        assert_eq!(header(&score(93), 0, 0.5), "Lead Guitar · E standard · 85 bpm · 4/4 · bar 1/93 · Intro");
        assert_eq!(header(&score(93), 2, 9.0), "Rhythm Guitar · Drop D · 85 bpm · 4/4 · bar 3/93 · Intro");
    }

    #[test]
    fn a_six_eight_bar_reads_six_eight() {
        let mut s = score(2);
        s.bars[1].time_sig = (6, 8);
        s.bars[1].len_q = 3.0;
        assert!(header(&s, 0, 4.5).contains(" · 6/8 · "), "{}", header(&s, 0, 4.5));
    }

    #[test]
    fn frets_in_a_fast_run_are_never_overwritten_or_merged() {
        // 64ths, 12 and 14 by turns on low E: a cell or less each at the widest scale.
        let mut s = score(2);
        s.parts[0].notes = (0..64).map(|i| note(i as f64 * 0.0625, 0.0625, 0, if i % 2 == 0 { 12 } else { 14 })).collect();
        let p = crate::theme::palette(ThemeKind::Classic);
        for step in 0..32 {
            let q = step as f64 * 0.1;
            let low = text(&tab_lines(&s, 0, q, 80, 20, false, None, TabLoop::Off, p)[6]);
            let numbers = low.split(|c: char| !c.is_ascii_digit()).filter(|t| !t.is_empty()).collect::<Vec<_>>();
            assert!(!numbers.is_empty(), "something shows at q {q}: {low:?}");
            assert!(numbers.iter().all(|t| *t == "12" || *t == "14"), "q {q}: {low:?}");
        }
    }

    #[test]
    fn the_parts_offered_skip_one_with_no_notes() {
        assert_eq!(playable_parts(&score(1)), vec![0, 2]);
    }

    #[test]
    fn a_late_tab_answer_for_the_last_track_is_dropped() {
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        ui.tab_gen = 5;
        ui.tab_audio = Some(PathBuf::from("/m/song.flac"));
        let (tx, rx) = std::sync::mpsc::channel();
        ui.tab_receiver = Some(rx);
        tx.send((4, Ok((PathBuf::from("/m/old.gp"), score(2))))).unwrap();
        take_answer(&mut ui);
        assert!(ui.tab.is_none(), "an older load's answer");
        let (tx, rx) = std::sync::mpsc::channel();
        ui.tab_receiver = Some(rx);
        tx.send((5, Ok((PathBuf::from("/m/song.gp"), score(2))))).unwrap();
        assert!(take_answer(&mut ui));
        assert!(ui.tab.is_some() && ui.tab_map.is_some());
        assert_eq!(ui.tab_sync.tab_file, "song.gp");
        // A saved part with no notes: the first one with some.
        let mut ui2 = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        ui2.tab_audio = Some(PathBuf::from("/m/song.flac"));
        ui2.tab_syncs.set(std::path::Path::new("/m/song.flac"), &crate::tab_sync::TrackSync { tab_file: "song.gp".into(), part: 1, ..Default::default() });
        let (tx, rx) = std::sync::mpsc::channel();
        ui2.tab_receiver = Some(rx);
        tx.send((0, Ok((PathBuf::from("/m/song.gp"), score(2))))).unwrap();
        take_answer(&mut ui2);
        assert_eq!(ui2.tab_sync.part, 0);
        // An unreadable file: the reason, in the panel and on the status line.
        let (tx, rx) = std::sync::mpsc::channel();
        ui2.tab_receiver = Some(rx);
        tx.send((0, Err("not a Guitar Pro 7/8 file".into()))).unwrap();
        take_answer(&mut ui2);
        assert_eq!(ui2.tab_error.as_deref(), Some("not a Guitar Pro 7/8 file"));
        assert!(ui2.tab.is_none());
    }

    #[test]
    fn the_tracker_runs_again_only_when_the_marks_change() {
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        let audio = PathBuf::from("/m/song.flac");
        ui.tab_audio = Some(audio.clone());
        ui.tab = Some(std::sync::Arc::new(score(8)));
        ui.tab_sync.marks = vec![(0, 1.0)];
        ui.tab_onsets = Some((audio, vec![0.0; 200 * 30]));
        remap(&mut ui);
        // What the tracker found, swapped for a value it would never find.
        let sentinel = vec![(0.0, 0.0), (8.0, 5.0)];
        ui.tab_corrections.as_mut().expect("cached").1 = sentinel.clone();
        let shifted = |ui: &UiState| ui.tab_map.as_ref().unwrap().secs(16.0).unwrap() - 1.0 - 16.0 * 60.0 / 85.0 - ui.tab_sync.nudge;
        for key in [KeyCode::Char('d'), KeyCode::Down, KeyCode::Char('a')] {
            assert!(handle_key(&mut ui, 0.0, key));
            assert!((shifted(&ui) - 5.0).abs() < 1e-9, "{key:?} reused the corrections: {}", shifted(&ui));
        }
        assert!(handle_key(&mut ui, 30.0, KeyCode::Enter));
        assert_ne!(ui.tab_corrections.as_ref().unwrap().1, sentinel, "a new mark tracks again");
        ui.tab_corrections.as_mut().unwrap().1 = sentinel.clone();
        assert!(handle_key(&mut ui, 0.0, KeyCode::Backspace));
        assert_ne!(ui.tab_corrections.as_ref().unwrap().1, sentinel, "undo tracks again");
    }

    #[test]
    fn the_onset_curve_moves_the_tab_and_a_mark_change_tracks_again() {
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        let audio = PathBuf::from("/m/song.flac");
        ui.tab_audio = Some(audio.clone());
        ui.tab = Some(std::sync::Arc::new(score(8)));
        ui.tab_sync.marks = vec![(0, 1.0)];
        remap(&mut ui);
        let plain = ui.tab_map.as_ref().unwrap().secs(16.0).unwrap();
        // Kicks 0.2 s after every beat of the map.
        let mut onsets = vec![0.0f32; 200 * 30];
        for q in 0..32 {
            onsets[((ui.tab_map.as_ref().unwrap().secs(q as f64).unwrap() + 0.2) * 200.0) as usize] = 1.0;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        ui.tab_beat_receiver = Some(rx);
        tx.send((ui.tab_gen + 1, Ok(onsets.clone()))).unwrap();
        assert!(!take_beats(&mut ui) && ui.tab_onsets.is_none(), "another load's curve");
        tx.send((ui.tab_gen, Ok(onsets))).unwrap();
        assert!(take_beats(&mut ui));
        let tracked = ui.tab_map.as_ref().unwrap().secs(16.0).unwrap();
        assert!((tracked - plain - 0.2).abs() < 0.01, "{tracked} vs {plain}");
        // Marks are kept: bar 1 has not moved. Backspace tracks again, from no mark.
        assert!((ui.tab_map.as_ref().unwrap().secs(0.0).unwrap() - 1.0).abs() < 1e-9);
        assert!(handle_key(&mut ui, 0.0, KeyCode::Backspace));
        assert!(ui.tab_map.as_ref().unwrap().secs(16.0).is_none());
        ui.tab_sync.marks = vec![(0, 1.0)];
        ui.tab_sync.nudge = 0.05;
        remap(&mut ui);
        let nudged = ui.tab_map.as_ref().unwrap().secs(16.0).unwrap();
        assert!((nudged - tracked - 0.05).abs() < 1e-9, "the nudge goes on top of the tracking");
    }

    #[test]
    fn every_part_shape_draws_in_every_size() {
        let mut sc = score(3);
        let tuning = |n: usize| (0..n).map(|i| 28 + 5 * i as u8).collect::<Vec<_>>();
        sc.parts = vec![
            Part { name: "Rests".into(), kind: PartKind::Guitar, tuning: tuning(6), notes: vec![] },
            Part { name: "Seven".into(), kind: PartKind::Guitar, tuning: tuning(7), notes: vec![note(0.0, 1.0, 6, 24), note(1.0, 0.5, 0, 3)] },
            Part { name: "Bass".into(), kind: PartKind::Bass, tuning: tuning(4), notes: vec![note(0.0, 4.0, 3, 5)] },
        ];
        let p = crate::theme::palette(ThemeKind::Classic);
        for part in 0..3 {
            for width in 1..120u16 {
                for rows in 1..30u16 {
                    for l in tab_lines(&sc, part, 1.5, width, rows, false, Some("sung ✓ 漢字"), TabLoop::Off, p) {
                        assert!(crate::tui::width(&text(&l)) <= width as usize);
                    }
                }
            }
        }
    }

    fn looping_ui() -> UiState {
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        ui.tab = Some(std::sync::Arc::new(score(8)));
        ui.tab_sync.marks = vec![(0, 0.0)];
        remap(&mut ui);
        ui
    }

    #[test]
    fn r_loops_from_one_bar_to_another_and_lets_go() {
        let bar = 4.0 * 60.0 / 85.0;
        let mut ui = looping_ui();
        assert!(handle_key(&mut ui, 2.5 * bar, KeyCode::Char('r')));
        assert_eq!(ui.tab_loop, TabLoop::From(2));
        // The second press in an earlier bar (after a seek back) still makes a loop.
        handle_key(&mut ui, 1.2 * bar, KeyCode::Char('r'));
        assert_eq!(ui.tab_loop, TabLoop::On { from: 1, to: 2, armed: false });
        handle_key(&mut ui, 1.5 * bar, KeyCode::Char('r'));
        assert_eq!(ui.tab_loop, TabLoop::Off);
        // Twice in one bar loops that bar.
        handle_key(&mut ui, 3.1 * bar, KeyCode::Char('r'));
        handle_key(&mut ui, 3.9 * bar, KeyCode::Char('r'));
        assert_eq!(ui.tab_loop, TabLoop::On { from: 3, to: 3, armed: false });
        // Unmarked, nothing to loop by: said, and not left to the player's rescan.
        let mut ui = looping_ui();
        ui.tab_sync.marks.clear();
        remap(&mut ui);
        assert!(handle_key(&mut ui, 1.0, KeyCode::Char('r')));
        assert_eq!(ui.tab_loop, TabLoop::Off);
    }

    #[test]
    fn a_loop_seeks_back_at_its_end_and_a_seek_out_of_it_lets_go() {
        let bar = 4.0 * 60.0 / 85.0;
        let mut ui = looping_ui();
        ui.tab_loop = TabLoop::On { from: 1, to: 2, armed: false };
        assert_eq!(loop_seek(&mut ui, 1.5 * bar), None);
        assert_eq!(loop_end(&ui), Some(3.0 * bar), "armed inside, wakes at its end");
        let back = loop_seek(&mut ui, 3.0 * bar + 0.01).expect("back at the end");
        assert!((back - (bar - 3.0 * bar - 0.01)).abs() < 1e-9, "{back}");
        // Until the seek lands the clock still reads past the end: no second seek.
        assert_eq!(loop_seek(&mut ui, 3.0 * bar + 0.05), None);
        assert_eq!(loop_seek(&mut ui, bar + 0.02), None);
        // A seek inside plays on; one out of it lets go.
        assert_eq!(loop_seek(&mut ui, 2.5 * bar), None);
        assert!(matches!(ui.tab_loop, TabLoop::On { armed: true, .. }));
        assert_eq!(loop_seek(&mut ui, 3.0 * bar + 10.0), None);
        assert_eq!(ui.tab_loop, TabLoop::Off);
    }

    #[test]
    fn a_loop_follows_its_bars_when_the_marks_move() {
        let bar = 4.0 * 60.0 / 85.0;
        let mut ui = looping_ui();
        ui.tab_loop = TabLoop::On { from: 1, to: 1, armed: true };
        assert_eq!(loop_end(&ui), Some(2.0 * bar));
        ui.tab_sync.marks = vec![(0, 1.0)];
        remap(&mut ui);
        assert_eq!(loop_end(&ui), Some(2.0 * bar + 1.0));
    }

    #[test]
    fn a_loops_ends_are_repeat_signs_over_their_bar_lines() {
        let p = crate::theme::palette(ThemeKind::Classic);
        let rows: Vec<String> = tab_lines(&score(8), 0, 4.5, 80, 20, false, None, TabLoop::On { from: 1, to: 1, armed: true }, p).iter().map(text).collect();
        let lines: Vec<usize> = rows[1].char_indices().filter(|(_, c)| matches!(c, '│' | '┃')).map(|(i, _)| rows[1][..i].chars().count()).collect();
        let (open, close) = (col(&rows[0], "‖:").expect("‖:"), col(&rows[0], ":‖").expect(":‖"));
        assert!(lines.contains(&open), "{:?} {:?}", rows[0], rows[1]);
        assert!(lines.contains(&(close + 1)), "{:?} {:?}", rows[0], rows[1]);
        assert!(close > open);
        let pending = tab_lines(&score(8), 0, 4.5, 80, 20, false, None, TabLoop::From(1), p);
        assert!(!text(&pending[0]).contains(":‖"));
    }

    #[test]
    fn zero_can_be_undone_with_backspace() {
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        ui.tab_audio = Some(PathBuf::from("/m/song.flac"));
        ui.tab = Some(std::sync::Arc::new(score(8)));
        ui.tab_sync.marks = vec![(0, 1.0), (1, 3.0)];
        ui.tab_sync.nudge = 0.15;
        let before = ui.tab_sync.clone();
        assert!(handle_key(&mut ui, 0.0, KeyCode::Char('0')));
        assert!(ui.tab_sync.marks.is_empty() && ui.tab_sync.nudge == 0.0);
        assert!(handle_key(&mut ui, 0.0, KeyCode::Backspace));
        assert_eq!(ui.tab_sync, before);
        let saved = ui.tab_syncs.get(std::path::Path::new("/m/song.flac"), "");
        assert_eq!(saved.marks, before.marks);
        assert!(ui.tab_cleared.is_none());
    }

    #[test]
    fn a_closed_panel_drops_the_tab_and_reopening_loads_again() {
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        let playlist = vec![PathBuf::from("/m/song.flac")];
        ui.view_mode = ViewMode::Tab;
        ui.tab_audio = Some(playlist[0].clone());
        ui.tab = Some(std::sync::Arc::new(score(2)));
        ui.tab_onsets = Some((playlist[0].clone(), vec![0.0; 10]));
        step(&mut ui, &playlist);
        assert!(ui.tab.is_some() && ui.tab_onsets.is_some(), "open keeps them");
        ui.view_mode = ViewMode::Player;
        step(&mut ui, &playlist);
        assert!(ui.tab.is_none() && ui.tab_onsets.is_none());
        ui.view_mode = ViewMode::Tab;
        assert_eq!(wanted(&ui, &playlist), Some(playlist[0].clone()));
    }
}
