//! Library tree view — artist → album → track browser (a second presentation
//! inside the `L` library view, toggled with Tab). See the design spec at
//! `docs/superpowers/specs/2026-07-03-library-tree-view-design.md`.
//!
//! This module holds the *pure* logic — building the tree from tag data,
//! projecting fold state into visible rows, and navigation — so it is unit
//! tested without a terminal or a live metadata cache. The renderer
//! (`render_library_tree`) is palette-driven and shared across themes.

/// Per-track tag inputs for `build`. The caller resolves these from the metadata
/// cache; `title` is already the display string (tag title or filename fallback).
/// The slice index IS the track's position in the current flat playlist.
#[derive(Clone, Debug)]
pub struct TrackTags {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub disc: Option<u32>,
    pub track: Option<u32>,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackRef {
    pub playlist_index: usize,
    pub title: String,
    pub track: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AlbumNode {
    pub name: String,
    pub tracks: Vec<TrackRef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtistNode {
    pub name: String,
    pub albums: Vec<AlbumNode>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LibraryTree {
    pub artists: Vec<ArtistNode>,
}

/// Group tracks into artist → album → track, sorted artist→album→disc→track→title
/// (case-insensitive for names). Untagged artist/album fall under
/// "Unknown Artist" / "Unknown Album". Same-name artists/albums that differ only
/// in case are merged, keeping the first-seen display spelling.
/// (artist, album, disc_sort, track, title, playlist_index) — the flat rows we
/// sort before folding into the nested tree.
type BuildRow<'a> = (&'a str, &'a str, u32, Option<u32>, &'a str, usize);

pub fn build(tags: &[TrackTags]) -> LibraryTree {
    // Names keep their display spelling; a parallel lowercase key drives
    // grouping/sorting so case variants merge and sort together.
    let mut rows: Vec<BuildRow> = tags
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let artist = t.artist.as_deref().unwrap_or("Unknown Artist");
            let album = t.album.as_deref().unwrap_or("Unknown Album");
            (artist, album, t.disc.unwrap_or(0), t.track, t.title.as_str(), i)
        })
        .collect();

    rows.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
            .then(a.2.cmp(&b.2))
            .then(a.3.unwrap_or(0).cmp(&b.3.unwrap_or(0)))
            .then_with(|| a.4.to_lowercase().cmp(&b.4.to_lowercase()))
    });

    // Fold consecutive same-name (case-insensitive) rows into nested nodes,
    // keeping the first-seen display spelling.
    let mut artists: Vec<ArtistNode> = Vec::new();
    for (artist, album, _disc, track, title, idx) in rows {
        // Compare by the same case-folded key the sort used, so case variants
        // (incl. non-ASCII) that sort adjacent also merge into one node.
        if artists
            .last()
            .is_none_or(|a| a.name.to_lowercase() != artist.to_lowercase())
        {
            artists.push(ArtistNode { name: artist.to_string(), albums: Vec::new() });
        }
        let anode = artists.last_mut().unwrap();
        if anode
            .albums
            .last()
            .is_none_or(|al| al.name.to_lowercase() != album.to_lowercase())
        {
            anode.albums.push(AlbumNode { name: album.to_string(), tracks: Vec::new() });
        }
        let alnode = anode.albums.last_mut().unwrap();
        alnode.tracks.push(TrackRef {
            playlist_index: idx,
            title: title.to_string(),
            track,
        });
    }

    LibraryTree { artists }
}

/// Which artists/albums are expanded, keyed by display name so the state
/// survives tree rebuilds (a rebuild after tags load doesn't slam folders shut).
#[derive(Clone, Debug, Default)]
pub struct FoldState {
    pub expanded_artists: std::collections::HashSet<String>,
    pub expanded_albums: std::collections::HashSet<(String, String)>,
}

/// A single on-screen row of the tree, in top-to-bottom order. Indices point
/// back into `LibraryTree` so the renderer and navigation can resolve the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisibleRow {
    Artist { artist: usize },
    Album { artist: usize, album: usize },
    Track { artist: usize, album: usize, track: usize },
}

/// Project fold state onto the tree: every artist row, then (if expanded) its
/// album rows, then (if that album is expanded) its track rows.
pub fn visible_rows(tree: &LibraryTree, fold: &FoldState) -> Vec<VisibleRow> {
    let mut rows = Vec::new();
    for (ai, artist) in tree.artists.iter().enumerate() {
        rows.push(VisibleRow::Artist { artist: ai });
        if !fold.expanded_artists.contains(&artist.name) {
            continue;
        }
        for (bi, album) in artist.albums.iter().enumerate() {
            rows.push(VisibleRow::Album { artist: ai, album: bi });
            let key = (artist.name.clone(), album.name.clone());
            if !fold.expanded_albums.contains(&key) {
                continue;
            }
            for ti in 0..album.tracks.len() {
                rows.push(VisibleRow::Track { artist: ai, album: bi, track: ti });
            }
        }
    }
    rows
}

/// The (artist, album) name key for an album row, used to index `expanded_albums`.
fn album_key(tree: &LibraryTree, artist: usize, album: usize) -> (String, String) {
    let a = &tree.artists[artist];
    (a.name.clone(), a.albums[album].name.clone())
}

/// Expand the header row's node (idempotent). No-op on a track row.
pub fn expand(tree: &LibraryTree, fold: &mut FoldState, row: VisibleRow) {
    match row {
        VisibleRow::Artist { artist } => {
            fold.expanded_artists.insert(tree.artists[artist].name.clone());
        }
        VisibleRow::Album { artist, album } => {
            fold.expanded_albums.insert(album_key(tree, artist, album));
        }
        VisibleRow::Track { .. } => {}
    }
}

/// Collapse the header row's node (idempotent). No-op on a track row.
pub fn collapse(tree: &LibraryTree, fold: &mut FoldState, row: VisibleRow) {
    match row {
        VisibleRow::Artist { artist } => {
            fold.expanded_artists.remove(&tree.artists[artist].name);
        }
        VisibleRow::Album { artist, album } => {
            fold.expanded_albums.remove(&album_key(tree, artist, album));
        }
        VisibleRow::Track { .. } => {}
    }
}

/// The playlist index `Enter` plays: the FIRST track under the row — a track →
/// itself, an album → its first track, an artist → their first album's first
/// track. `None` only for a structurally-empty node.
pub fn first_track_index(tree: &LibraryTree, row: VisibleRow) -> Option<usize> {
    match row {
        VisibleRow::Track { artist, album, track } => {
            Some(tree.artists[artist].albums[album].tracks[track].playlist_index)
        }
        VisibleRow::Album { artist, album } => {
            tree.artists[artist].albums[album].tracks.first().map(|t| t.playlist_index)
        }
        VisibleRow::Artist { artist } => tree.artists[artist]
            .albums
            .iter()
            .flat_map(|al| al.tracks.first())
            .map(|t| t.playlist_index)
            .next(),
    }
}

/// All playlist indices under a row, in tree order — a track → `[self]`, an
/// album → its tracks, an artist → all their tracks. Used by remove.
pub fn subtree_track_indices(tree: &LibraryTree, row: VisibleRow) -> Vec<usize> {
    match row {
        VisibleRow::Track { artist, album, track } => {
            vec![tree.artists[artist].albums[album].tracks[track].playlist_index]
        }
        VisibleRow::Album { artist, album } => tree.artists[artist].albums[album]
            .tracks
            .iter()
            .map(|t| t.playlist_index)
            .collect(),
        VisibleRow::Artist { artist } => tree.artists[artist]
            .albums
            .iter()
            .flat_map(|al| al.tracks.iter())
            .map(|t| t.playlist_index)
            .collect(),
    }
}

/// Filtered visible rows: a track is shown when `query` (lowercased substring)
/// matches its title, album, or artist. Matching branches are kept and fully
/// expanded (fold ignored); non-matching branches are pruned. Empty query
/// matches everything (the caller uses fold-based `visible_rows` instead).
pub fn visible_rows_filtered(tree: &LibraryTree, query: &str) -> Vec<VisibleRow> {
    let q = query.to_lowercase();
    let mut rows = Vec::new();
    for (ai, artist) in tree.artists.iter().enumerate() {
        let artist_match = artist.name.to_lowercase().contains(&q);
        let mut artist_rows = Vec::new();
        for (bi, album) in artist.albums.iter().enumerate() {
            let album_match = artist_match || album.name.to_lowercase().contains(&q);
            let mut album_rows = Vec::new();
            for (ti, track) in album.tracks.iter().enumerate() {
                if album_match || track.title.to_lowercase().contains(&q) {
                    album_rows.push(VisibleRow::Track { artist: ai, album: bi, track: ti });
                }
            }
            if !album_rows.is_empty() {
                artist_rows.push(VisibleRow::Album { artist: ai, album: bi });
                artist_rows.extend(album_rows);
            }
        }
        if !artist_rows.is_empty() {
            rows.push(VisibleRow::Artist { artist: ai });
            rows.extend(artist_rows);
        }
    }
    rows
}

use crate::ansi::{truncate_visible, visible_len};

/// Render the tree body as indented lines using the active theme palette. Shared
/// across all themes — a tree is structurally identical everywhere; only colors
/// and the cursor tint differ. Renders the window `[scroll, scroll+height)` of
/// `visible`; the caller supplies `visible`, `cursor`, `scroll` (its own tree
/// state) and pads/positions the block within its chrome.
#[allow(clippy::too_many_arguments)] // cohesive render context
pub fn render_library_tree(
    tree: &LibraryTree,
    fold: &FoldState,
    visible: &[VisibleRow],
    cursor: usize,
    scroll: usize,
    height: usize,
    width: usize,
    p: &crate::theme::Palette,
    now_playing: Option<usize>,
) -> Vec<String> {
    let end = (scroll + height).min(visible.len());
    let mut out = Vec::with_capacity(end.saturating_sub(scroll));
    for (vi, &vrow) in visible.iter().enumerate().skip(scroll).take(end - scroll) {
        // Two forms per row: `plain` (no color, for the highlighted cursor row —
        // inner color/reset codes would cancel the highlight) and `colored`.
        let (plain, colored) = match vrow {
            VisibleRow::Artist { artist } => {
                let a = &tree.artists[artist];
                let g = if fold.expanded_artists.contains(&a.name) { '▾' } else { '▸' };
                (
                    format!("  {} {}", g, a.name),
                    format!("  {}{} {}{}", p.fg, g, a.name, p.reset),
                )
            }
            VisibleRow::Album { artist, album } => {
                let al = &tree.artists[artist].albums[album];
                let g = if fold.expanded_albums.contains(&album_key(tree, artist, album)) {
                    '▾'
                } else {
                    '▸'
                };
                (
                    format!("    {} {}", g, al.name),
                    format!("    {}{} {}{}", p.dim, g, al.name, p.reset),
                )
            }
            VisibleRow::Track { artist, album, track } => {
                let t = &tree.artists[artist].albums[album].tracks[track];
                let now = now_playing == Some(t.playlist_index);
                let num = t.track.map(|n| format!("{n:02}")).unwrap_or_else(|| "··".into());
                let marker = if now { '▶' } else { ' ' };
                let col = if now { p.accent } else { p.fg };
                (
                    format!("      {} {} {}", num, marker, t.title),
                    format!("      {}{} {}{} {}{}", p.dim, num, col, marker, t.title, p.reset),
                )
            }
        };
        let line = if vi == cursor {
            // Highlight the whole row. Use the theme's cursor tint when it has
            // one; otherwise (e.g. Classic) reverse-video, matching the flat
            // list. Plain text so no inner reset cancels the highlight.
            let plain = truncate_visible(&plain, width);
            let pad = " ".repeat(width.saturating_sub(visible_len(&plain)));
            if p.cursor_bg.is_empty() {
                format!("\x1B[7m{plain}{pad}\x1B[27m")
            } else {
                format!("{}{plain}{pad}{}", p.cursor_bg, p.reset)
            }
        } else {
            truncate_visible(&colored, width)
        };
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(
        artist: Option<&str>,
        album: Option<&str>,
        disc: Option<u32>,
        track: Option<u32>,
        title: &str,
    ) -> TrackTags {
        TrackTags {
            artist: artist.map(String::from),
            album: album.map(String::from),
            disc,
            track,
            title: title.into(),
        }
    }

    #[test]
    fn build_groups_by_artist_then_album_sorted_with_unknown_bucket() {
        let tags = vec![
            tag(Some("Radiohead"), Some("OK Computer"), None, Some(2), "Paranoid Android"), // 0
            tag(Some("Arctic Monkeys"), Some("AM"), None, Some(1), "Do I Wanna Know?"),      // 1
            tag(Some("Radiohead"), Some("OK Computer"), None, Some(1), "Airbag"),            // 2
            tag(None, None, None, None, "field_recording.wav"),                              // 3
            tag(Some("arctic monkeys"), Some("AM"), None, Some(2), "R U Mine?"),             // 4
        ];
        let tree = build(&tags);

        // Artists sorted case-insensitively, Unknown last (u > r > a alphabetically).
        let names: Vec<&str> = tree.artists.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Arctic Monkeys", "Radiohead", "Unknown Artist"]);

        // "arctic monkeys" merges into "Arctic Monkeys" (first-seen spelling), both
        // tracks under AM in track order → playlist indices 1 then 4.
        let am = &tree.artists[0].albums[0];
        assert_eq!(am.name, "AM");
        assert_eq!(
            am.tracks.iter().map(|t| t.playlist_index).collect::<Vec<_>>(),
            vec![1, 4]
        );

        // OK Computer sorted by track number: Airbag (t1, idx2) before Paranoid (t2, idx0).
        let okc = &tree.artists[1].albums[0];
        assert_eq!(
            okc.tracks.iter().map(|t| t.playlist_index).collect::<Vec<_>>(),
            vec![2, 0]
        );

        // Untagged → Unknown Artist / Unknown Album.
        assert_eq!(tree.artists[2].name, "Unknown Artist");
        assert_eq!(tree.artists[2].albums[0].name, "Unknown Album");
        assert_eq!(tree.artists[2].albums[0].tracks[0].playlist_index, 3);
    }

    fn sample_tree() -> LibraryTree {
        build(&[
            tag(Some("Arctic Monkeys"), Some("AM"), None, Some(1), "Do I Wanna Know?"),
            tag(Some("Arctic Monkeys"), Some("AM"), None, Some(2), "R U Mine?"),
            tag(Some("Radiohead"), Some("OK Computer"), None, Some(1), "Airbag"),
            tag(Some("Radiohead"), Some("OK Computer"), None, Some(2), "Paranoid Android"),
        ])
    }

    #[test]
    fn visible_rows_collapsed_shows_only_artists() {
        let tree = sample_tree();
        let rows = visible_rows(&tree, &FoldState::default());
        assert_eq!(
            rows,
            vec![
                VisibleRow::Artist { artist: 0 },
                VisibleRow::Artist { artist: 1 },
            ]
        );
    }

    #[test]
    fn visible_rows_expanding_artist_reveals_albums_only() {
        let tree = sample_tree();
        let mut fold = FoldState::default();
        fold.expanded_artists.insert("Arctic Monkeys".into());
        let rows = visible_rows(&tree, &fold);
        assert_eq!(
            rows,
            vec![
                VisibleRow::Artist { artist: 0 },
                VisibleRow::Album { artist: 0, album: 0 },
                VisibleRow::Artist { artist: 1 },
            ]
        );
    }

    #[test]
    fn visible_rows_expanding_album_reveals_its_tracks() {
        let tree = sample_tree();
        let mut fold = FoldState::default();
        fold.expanded_artists.insert("Arctic Monkeys".into());
        fold.expanded_albums.insert(("Arctic Monkeys".into(), "AM".into()));
        let rows = visible_rows(&tree, &fold);
        assert_eq!(
            rows,
            vec![
                VisibleRow::Artist { artist: 0 },
                VisibleRow::Album { artist: 0, album: 0 },
                VisibleRow::Track { artist: 0, album: 0, track: 0 },
                VisibleRow::Track { artist: 0, album: 0, track: 1 },
                VisibleRow::Artist { artist: 1 },
            ]
        );
    }

    #[test]
    fn expand_then_collapse_a_header() {
        let tree = sample_tree();
        let mut fold = FoldState::default();
        let artist_row = VisibleRow::Artist { artist: 0 };

        expand(&tree, &mut fold, artist_row);
        assert!(fold.expanded_artists.contains("Arctic Monkeys"));
        assert_eq!(visible_rows(&tree, &fold).len(), 3); // artist0 + its album + artist1

        collapse(&tree, &mut fold, artist_row);
        assert!(!fold.expanded_artists.contains("Arctic Monkeys"));
        assert_eq!(visible_rows(&tree, &fold).len(), 2); // back to just artists
    }

    #[test]
    fn expand_collapse_on_track_is_noop() {
        let tree = sample_tree();
        let mut fold = FoldState::default();
        fold.expanded_artists.insert("Arctic Monkeys".into());
        let (a, b) = (fold.expanded_artists.clone(), fold.expanded_albums.clone());
        let track = VisibleRow::Track { artist: 0, album: 0, track: 0 };
        expand(&tree, &mut fold, track);
        collapse(&tree, &mut fold, track);
        assert_eq!(fold.expanded_artists, a);
        assert_eq!(fold.expanded_albums, b);
    }


    #[test]
    fn render_shows_fold_glyphs_indented_names_and_windows_correctly() {
        let tree = sample_tree();
        let mut fold = FoldState::default();
        fold.expanded_artists.insert("Arctic Monkeys".into());
        let vis = visible_rows(&tree, &fold); // [AM artist, AM album, Radiohead artist]
        let p = crate::theme::palette(crate::theme::ThemeKind::Classic);
        let lines = render_library_tree(&tree, &fold, &vis, 0, 0, 10, 40, p, None);

        assert_eq!(lines.len(), vis.len()); // one line per visible row within the window
        // Expanded artist shows ▾, collapsed shows ▸; names present.
        assert!(lines[0].contains('▾') && lines[0].contains("Arctic Monkeys"));
        assert!(lines[2].contains('▸') && lines[2].contains("Radiohead"));
        // Album row indented deeper than the artist row (more leading spaces).
        let indent = |s: &str| s.chars().take_while(|c| *c == ' ').count();
        // strip ANSI first: the visible prefix spaces come after the SGR codes,
        // so compare the raw byte position of the name instead.
        assert!(lines[1].find("AM").unwrap() > lines[0].find("Arctic Monkeys").unwrap());
        let _ = indent;

        // Windowing: height 1 from scroll 1 yields just the album row.
        let one = render_library_tree(&tree, &fold, &vis, 1, 1, 1, 40, p, None);
        assert_eq!(one.len(), 1);
        assert!(one[0].contains("AM"));
    }

    #[test]
    fn first_track_index_for_each_row_type() {
        let tree = sample_tree();
        // Track → itself (AM track 1 = playlist idx 1).
        assert_eq!(
            first_track_index(&tree, VisibleRow::Track { artist: 0, album: 0, track: 1 }),
            Some(1)
        );
        // Album → its first track (AM first = idx 0).
        assert_eq!(
            first_track_index(&tree, VisibleRow::Album { artist: 0, album: 0 }),
            Some(0)
        );
        // Artist → first album's first track (Radiohead → OK Computer → Airbag = idx 2).
        assert_eq!(first_track_index(&tree, VisibleRow::Artist { artist: 1 }), Some(2));
    }

    #[test]
    fn subtree_track_indices_for_each_row_type() {
        let tree = sample_tree();
        assert_eq!(
            subtree_track_indices(&tree, VisibleRow::Track { artist: 0, album: 0, track: 1 }),
            vec![1]
        );
        assert_eq!(
            subtree_track_indices(&tree, VisibleRow::Album { artist: 0, album: 0 }),
            vec![0, 1]
        );
        assert_eq!(
            subtree_track_indices(&tree, VisibleRow::Artist { artist: 1 }),
            vec![2, 3]
        );
    }

    #[test]
    fn filter_matches_at_each_level_keeps_ancestors_prunes_rest() {
        let tree = sample_tree();
        // Artist-level: "arctic" → Arctic Monkeys + AM + both tracks.
        assert_eq!(
            visible_rows_filtered(&tree, "arctic"),
            vec![
                VisibleRow::Artist { artist: 0 },
                VisibleRow::Album { artist: 0, album: 0 },
                VisibleRow::Track { artist: 0, album: 0, track: 0 },
                VisibleRow::Track { artist: 0, album: 0, track: 1 },
            ]
        );
        // Album-level: "ok computer" → Radiohead + OK Computer + both tracks.
        assert_eq!(
            visible_rows_filtered(&tree, "OK Computer"),
            vec![
                VisibleRow::Artist { artist: 1 },
                VisibleRow::Album { artist: 1, album: 0 },
                VisibleRow::Track { artist: 1, album: 0, track: 0 },
                VisibleRow::Track { artist: 1, album: 0, track: 1 },
            ]
        );
        // Track-level: "airbag" → just Radiohead › OK Computer › Airbag.
        assert_eq!(
            visible_rows_filtered(&tree, "airbag"),
            vec![
                VisibleRow::Artist { artist: 1 },
                VisibleRow::Album { artist: 1, album: 0 },
                VisibleRow::Track { artist: 1, album: 0, track: 0 },
            ]
        );
        // No match → empty.
        assert!(visible_rows_filtered(&tree, "zzzz").is_empty());
    }

    #[test]
    fn cursor_row_is_highlighted_in_every_theme() {
        use crate::theme::ThemeKind;
        let tree = sample_tree();
        let fold = FoldState::default();
        let vis = visible_rows(&tree, &fold);
        for kind in [ThemeKind::Classic, ThemeKind::Minimal, ThemeKind::HiFi] {
            let p = crate::theme::palette(kind);
            let lines = render_library_tree(&tree, &fold, &vis, 0, 0, 5, 40, p, None);
            // Row 0 is the cursor: it must carry a highlight — reverse-video when
            // the theme has no cursor tint (Classic), or the tint otherwise.
            let highlighted = lines[0].contains("\x1B[7m")
                || (!p.cursor_bg.is_empty() && lines[0].contains(p.cursor_bg));
            assert!(highlighted, "{kind:?}: cursor row not highlighted: {:?}", lines[0]);
            // A non-cursor row must not be reverse-video highlighted.
            assert!(!lines[1].contains("\x1B[7m"), "{kind:?}: non-cursor row highlighted");
        }
    }
}
