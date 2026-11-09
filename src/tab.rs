//! The score model the guitar-tab panel draws from: pure data, built by
//! `gp::parse_gpif`. Bars are PLAYED bars (repeats already unrolled), times are
//! in quarter notes from the start of the song.

/// A song: its played bars and the parts (tracks) that have notes in them.
#[derive(Debug)]
pub struct Score {
    #[allow(dead_code)] // read from the file, shown nowhere yet (no task in the plan uses it)
    pub title: String,
    #[allow(dead_code)] // as `title`
    pub artist: String,
    pub bars: Vec<Bar>,
    pub parts: Vec<Part>,
    /// Sung syllables (quarters, text), from the first track that has any.
    pub lyrics: Vec<(f64, String)>,
}

#[derive(Debug)]
pub struct Bar {
    pub start_q: f64,
    pub len_q: f64,
    /// The file's time signature (num, den): 6/8 and 3/4 have the same length.
    pub time_sig: (u8, u8),
    /// (quarters from the bar's start, bpm); the first is at 0.0.
    pub tempos: Vec<(f64, f64)>,
    pub section: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PartKind {
    Guitar,
    Bass,
    OtherFretted,
    Drums,
}

#[derive(Debug)]
pub struct Part {
    pub name: String,
    pub kind: PartKind,
    /// MIDI pitches, lowest string first.
    pub tuning: Vec<u8>,
    pub notes: Vec<Note>,
}

#[derive(Debug)]
pub struct Note {
    pub q: f64,
    pub len_q: f64,
    /// 0 = the lowest string.
    pub string: u8,
    pub fret: u8,
    /// A tie's destination: sounding, but not a new onset.
    pub tied: bool,
    pub dots: u8,
    /// (num, den): `num` notes in the time of `den`.
    pub tuplet: Option<(u8, u8)>,
    pub fx: Effects,
}

/// A note's techniques, drawn round its fret as ASCII tab writes them.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Effects {
    /// A hammer-on or pull-off to the next note on the string.
    pub hopo: bool,
    /// Guitar Pro's slide flags: 1 or 2 into the next note, 4 out down,
    /// 8 out up, 16 in from below, 32 in from above.
    pub slide: u8,
    pub bend: bool,
    /// The bend comes back down.
    pub release: bool,
    pub vibrato: bool,
    /// A dead (muted) note: `x`, no pitch.
    pub dead: bool,
    pub harmonic: bool,
    pub palm_mute: bool,
    pub let_ring: bool,
}

impl Score {
    /// Part indices the panel offers: guitar and bass first (file order), then
    /// the other fretted ones; drums and parts with no strings left out.
    pub fn tab_parts(&self) -> Vec<usize> {
        let with = |f: fn(PartKind) -> bool| {
            self.parts
                .iter()
                .enumerate()
                .filter(move |(_, p)| !p.tuning.is_empty() && f(p.kind))
                .map(|(i, _)| i)
        };
        with(|k| matches!(k, PartKind::Guitar | PartKind::Bass))
            .chain(with(|k| k == PartKind::OtherFretted))
            .collect()
    }

    pub fn len_q(&self) -> f64 {
        self.bars.last().map_or(0.0, |b| b.start_q + b.len_q)
    }

    /// The played bar holding `q` (the last bar for a `q` past the end).
    pub fn bar_at(&self, q: f64) -> usize {
        self.bars.partition_point(|b| b.start_q <= q).saturating_sub(1)
    }
}

const NAMES: [&str; 12] = ["C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B"];

/// "E", "G♯": the pitch class of a MIDI note.
pub fn note_name(midi: u8) -> String {
    NAMES[midi as usize % 12].to_string()
}

/// "A2": with the octave (MIDI 45 = A2).
#[allow(dead_code)] // the panel's `n` shows pitch classes (two cells); no task in the plan uses this yet
pub fn note_name_oct(midi: u8) -> String {
    format!("{}{}", note_name(midi), midi as i32 / 12 - 1)
}

/// "E standard", "Drop D", "E♭ standard", else the notes, lowest string first.
pub fn tuning_name(tuning: &[u8]) -> String {
    const STANDARDS: [&[u8]; 2] = [&[40, 45, 50, 55, 59, 64], &[28, 33, 38, 43]];
    const DROP_D: [u8; 6] = [38, 45, 50, 55, 59, 64];
    if tuning == DROP_D {
        return "Drop D".into();
    }
    for std in STANDARDS {
        if tuning.len() == std.len() {
            if tuning == std {
                return "E standard".into();
            }
            if tuning.iter().zip(std).all(|(&t, &s)| s.checked_sub(1) == Some(t)) {
                return "E♭ standard".into();
            }
        }
    }
    tuning.iter().map(|&m| note_name(m)).collect::<Vec<_>>().join(" ")
}

use std::path::{Path, PathBuf};

/// The Guitar Pro file that belongs to a track: the one named like it, else a `.gp`
/// in the same folder whose name contains the title (the artist breaks ties).
pub fn find_tab(audio: &Path, title: Option<&str>, artist: Option<&str>) -> Option<PathBuf> {
    let dir = audio.parent()?;
    let own = ["gp", "GP"].map(|e| audio.with_extension(e));
    if let Some(p) = own.into_iter().find(|p| p.is_file()) {
        return Some(p);
    }
    let norm = |s: &str| -> String { s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect() };
    let title = norm(title?);
    if title.is_empty() {
        return None;
    }
    let artist = artist.map(norm).filter(|a| !a.is_empty());
    let both = format!("{}{title}", artist.as_deref().unwrap_or(""));
    // Ranked: the stem is the title (or artist + title), ends with it, or merely contains it.
    let mut hits: Vec<(u8, bool, usize, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("gp")) && p.is_file())
        .filter_map(|p| {
            let stem = norm(&p.file_stem()?.to_string_lossy());
            let rank = if stem == title || stem == both {
                0
            } else if stem.ends_with(&title) {
                1
            } else if stem.contains(&title) && title.chars().count() >= 4 {
                2 // a short title inside a longer word is too likely another song
            } else {
                return None;
            };
            let has_artist = artist.as_ref().is_some_and(|a| stem.contains(a));
            Some((rank, !has_artist, stem.len(), p))
        })
        .collect();
    hits.sort();
    hits.into_iter().next().map(|h| h.3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuning_and_note_names() {
        assert_eq!(tuning_name(&[40, 45, 50, 55, 59, 64]), "E standard");
        assert_eq!(tuning_name(&[38, 45, 50, 55, 59, 64]), "Drop D");
        assert_eq!(tuning_name(&[39, 44, 49, 54, 58, 63]), "E♭ standard");
        assert_eq!(tuning_name(&[38, 45, 50, 55, 59, 62]), "D A D G B D");
        assert_eq!(note_name(68), "G♯");
        assert_eq!(note_name_oct(45), "A2");
    }

    #[test]
    fn bar_lookup() {
        let bar = |s: f64| Bar { start_q: s, len_q: 4.0, time_sig: (4, 4), tempos: vec![(0.0, 90.0)], section: None };
        let s = Score { title: String::new(), artist: String::new(), bars: vec![bar(0.0), bar(4.0)], parts: vec![], lyrics: vec![] };
        assert_eq!((s.bar_at(-1.0), s.bar_at(3.9), s.bar_at(4.0), s.bar_at(99.0)), (0, 0, 1, 1));
        assert_eq!(s.len_q(), 8.0);
    }

    fn tab_dir(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keet-tab-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        dir
    }

    #[test]
    fn the_tab_named_like_the_track_wins() {
        let dir = tab_dir("named", &["song.flac", "song.gp", "Other - Song.gp"]);
        let got = find_tab(&dir.join("song.flac"), Some("Song"), None);
        assert_eq!(got, Some(dir.join("song.gp")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tab_titled_like_the_track_is_found() {
        let dir = tab_dir("titled", &["01. Do I Wanna Know.flac", "Arctic_Monkeys_-_Do_I_Wanna_Know.GP"]);
        let got = find_tab(&dir.join("01. Do I Wanna Know.flac"), Some("Do I Wanna Know?"), None);
        assert_eq!(got, Some(dir.join("Arctic_Monkeys_-_Do_I_Wanna_Know.GP")));
        assert_eq!(find_tab(&dir.join("01. Do I Wanna Know.flac"), Some("?!"), None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_titled_tabs_take_the_one_with_the_artist() {
        let dir = tab_dir("two", &["a.flac", "U2_-_Velvet.gp", "Cover_-_Velvet.gp"]);
        let a = dir.join("a.flac");
        assert_eq!(find_tab(&a, Some("Velvet"), Some("U2")), Some(dir.join("U2_-_Velvet.gp")));
        assert_eq!(find_tab(&a, Some("Velvet"), None), Some(dir.join("U2_-_Velvet.gp")), "the shortest stem");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_short_title_is_not_found_inside_another_word() {
        let dir = tab_dir("short", &["a.flac", "Money.gp", "Metallica_-_One.gp"]);
        let a = dir.join("a.flac");
        assert_eq!(find_tab(&a, Some("One"), Some("Metallica")), Some(dir.join("Metallica_-_One.gp")));
        assert_eq!(find_tab(&a, Some("One"), None), Some(dir.join("Metallica_-_One.gp")));
        let _ = std::fs::remove_dir_all(&dir);
        let dir = tab_dir("short2", &["a.flac", "Money.gp"]);
        assert_eq!(find_tab(&dir.join("a.flac"), Some("One"), None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_tab_is_none() {
        let dir = tab_dir("none", &["a.flac", "notes.txt"]);
        assert_eq!(find_tab(&dir.join("a.flac"), Some("Velvet"), None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
