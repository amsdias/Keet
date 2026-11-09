//! Guitar Pro 7/8 files (`.gp`): a zip archive whose `Content/score.gpif` is
//! the score as XML. This is the zip half: find one entry and inflate it,
//! without trusting a single offset or size in the file.

use std::io::Read;

const EOCD_SIG: u32 = 0x0605_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;
/// The end record is 22 bytes plus a comment of at most 65,535.
const EOCD_SEARCH: usize = 65_557;

fn u16_at(b: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(at..at.checked_add(2)?)?.try_into().ok()?) as usize)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

/// The bytes of the entry `name`, at most `cap` of them (declared and actual).
pub(crate) fn zip_entry(zip: &[u8], name: &str, cap: u64) -> Result<Vec<u8>, String> {
    let bad = || "not a valid zip file".to_string();
    let eocd = (0..=zip.len().saturating_sub(22))
        .rev()
        .take(EOCD_SEARCH - 21)
        .find(|&i| u32_at(zip, i) == Some(EOCD_SIG))
        .ok_or_else(bad)?;
    let count = u16_at(zip, eocd + 10).ok_or_else(bad)?;
    let mut at = u32_at(zip, eocd + 16).ok_or_else(bad)? as usize;
    for _ in 0..count {
        if u32_at(zip, at) != Some(CENTRAL_SIG) {
            return Err(bad());
        }
        let method = u16_at(zip, at + 10).ok_or_else(bad)?;
        let packed = u32_at(zip, at + 20).ok_or_else(bad)? as u64;
        let size = u32_at(zip, at + 24).ok_or_else(bad)? as u64;
        let (n, m, k) = (
            u16_at(zip, at + 28).ok_or_else(bad)?,
            u16_at(zip, at + 30).ok_or_else(bad)?,
            u16_at(zip, at + 32).ok_or_else(bad)?,
        );
        let local = u32_at(zip, at + 42).ok_or_else(bad)? as usize;
        let entry_name = zip.get(at + 46..at + 46 + n).ok_or_else(bad)?;
        at = at + 46 + n + m + k;
        if entry_name != name.as_bytes() {
            continue;
        }
        if size > cap || (method == 0 && packed > cap) {
            return Err(format!("{name} is too large"));
        }
        if u32_at(zip, local) != Some(LOCAL_SIG) {
            return Err(bad());
        }
        let start = local + 30 + u16_at(zip, local + 26).ok_or_else(bad)? + u16_at(zip, local + 28).ok_or_else(bad)?;
        let data = zip.get(start..start.checked_add(packed as usize).ok_or_else(bad)?).ok_or_else(bad)?;
        let mut out = Vec::new();
        match method {
            0 => out.extend_from_slice(data),
            8 => {
                // One byte past the cap tells a lying directory from a full entry.
                flate2::read::DeflateDecoder::new(data)
                    .take(cap + 1)
                    .read_to_end(&mut out)
                    .map_err(|e| format!("{name}: {e}"))?;
            }
            other => return Err(format!("compression method {other}")),
        }
        if out.len() as u64 > cap {
            return Err(format!("{name} is too large"));
        }
        return Ok(out);
    }
    Err(format!("{name} is not in the file"))
}

use crate::ansi::sanitize_display;
use crate::tab::{Bar, Effects, Note, Part, PartKind, Score};
use roxmltree::Node;
use std::collections::HashMap;
use std::path::Path;

const MAX_FILE: u64 = 32 * 1024 * 1024;
const MAX_ENTRY: u64 = 64 * 1024 * 1024;
const MAX_FRET: i64 = 30;
const MAX_STRINGS: usize = 10;
const MAX_BARS: usize = 5_000;
const MAX_NOTES: usize = 200_000;
const MAX_BEATS: usize = 1_000_000;
const MAX_TRACKS: usize = 64;
const MAX_NODES: u32 = 5_000_000;

/// Read a `.gp` file (Guitar Pro 7/8) into the score model.
pub fn read_score(path: &Path) -> Result<Score, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_FILE {
        return Err("file is too large for a Guitar Pro score".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let xml = zip_entry(&bytes, "Content/score.gpif", MAX_ENTRY)?;
    parse_gpif(std::str::from_utf8(&xml).map_err(|_| "score.gpif is not UTF-8".to_string())?)
}

fn kid<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(name))
}

fn kid_text<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    kid(n, name).and_then(|c| c.text()).map(str::trim)
}

fn need<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Result<Node<'a, 'i>, String> {
    kid(n, name).ok_or_else(|| format!("score has no <{name}>"))
}

/// The `Property` named `name` under `n`'s `Properties`.
fn property<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    kid(n, "Properties")?.children().find(|p| p.has_tag_name("Property") && p.attribute("name") == Some(name))
}

fn ids(s: Option<&str>) -> impl Iterator<Item = &str> {
    s.unwrap_or("").split_whitespace()
}

/// Elements of `<parent><child id=..>` by id.
fn by_id<'a, 'i>(root: Node<'a, 'i>, parent: &str, child: &str) -> HashMap<&'a str, Node<'a, 'i>> {
    let mut m = HashMap::new();
    if let Some(p) = kid(root, parent) {
        for c in p.children().filter(|c| c.has_tag_name(child)) {
            if let Some(id) = c.attribute("id") {
                m.insert(id, c);
            }
        }
    }
    m
}

struct Master<'a> {
    node: Node<'a, 'a>,
    len_q: f64,
    time_sig: (u8, u8),
    start: bool,
    end: bool,
    count: u32,
    endings: Vec<u32>,
}

/// Master-bar indices in the order the song plays. A repeat start with no end
/// repeats nothing; an end with no start goes back to the first bar.
fn unroll(masters: &[Master]) -> Result<Vec<usize>, String> {
    let (mut out, mut i, mut region, mut pass, mut jumped) = (Vec::new(), 0, 0, 1, false);
    while i < masters.len() {
        let m = &masters[i];
        if m.start && !std::mem::take(&mut jumped) {
            region = i;
            pass = 1;
        }
        jumped = false;
        i += 1;
        if !m.endings.is_empty() && !m.endings.contains(&pass) {
            continue;
        }
        out.push(i - 1);
        if out.len() > MAX_BARS {
            return Err(format!("more than {MAX_BARS} bars"));
        }
        if m.end {
            if pass < m.count.max(2) {
                pass += 1;
                i = region;
                jumped = true;
            } else {
                pass = 1;
            }
        } else if !m.endings.is_empty() && masters.get(i).is_none_or(|n| n.endings.is_empty()) {
            // Past the endings: a later end with no start repeats from here, as a first pass.
            pass = 1;
            region = i;
        }
    }
    Ok(out)
}

/// (quarters, dots, tuplet) of a `Rhythm`.
type Timing = (f64, u8, Option<(u8, u8)>);

fn rhythm(r: Node) -> Result<Timing, String> {
    let base = match kid_text(r, "NoteValue") {
        Some("Whole") => 4.0,
        Some("Half") => 2.0,
        Some("Quarter") => 1.0,
        Some("Eighth") => 0.5,
        Some("16th") => 0.25,
        Some("32nd") => 0.125,
        Some("64th") => 0.0625,
        Some("128th") => 0.03125,
        other => return Err(format!("unknown note value {other:?}")),
    };
    let dots: u8 = kid(r, "AugmentationDot").and_then(|d| d.attribute("count")).and_then(|c| c.parse().ok()).unwrap_or(0);
    let tuplet = kid(r, "PrimaryTuplet").and_then(|t| {
        let n: u8 = t.attribute("num")?.parse().ok()?;
        let d: u8 = t.attribute("den")?.parse().ok()?;
        (n > 0 && d > 0 && n != d).then_some((n, d))
    });
    let mut len = base * (2.0 - 0.5f64.powi(dots.min(8) as i32));
    if let Some((n, d)) = tuplet {
        len *= d as f64 / n as f64;
    }
    Ok((len, dots, tuplet))
}

/// The techniques GPIF marks on a note.
fn effects(n: Node) -> Effects {
    let on = |p: &str| property(n, p).is_some_and(|p| kid(p, "Enable").is_some());
    let value = |p: &str| property(n, p).and_then(|p| kid_text(p, "Float")).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let bend = on("Bended");
    Effects {
        hopo: on("HopoOrigin"),
        slide: property(n, "Slide").and_then(|p| kid_text(p, "Flags")).and_then(|f| f.parse().ok()).unwrap_or(0),
        bend,
        release: bend && value("BendDestinationValue") < value("BendOriginValue").max(value("BendMiddleValue")),
        vibrato: kid(n, "Vibrato").is_some(),
        dead: on("Muted"),
        harmonic: property(n, "HarmonicType").and_then(|p| kid_text(p, "HType")).is_some_and(|t| t != "NoHarmonic"),
        palm_mute: on("PalmMuted"),
        let_ring: kid(n, "LetRing").is_some(),
    }
}

fn instrument_kind(itype: &str, has_tuning: bool) -> PartKind {
    let t = itype.to_ascii_lowercase();
    if t.contains("drum") {
        PartKind::Drums
    } else if t.contains("bass") {
        PartKind::Bass
    } else if t.contains("guitar") {
        PartKind::Guitar
    } else if has_tuning {
        PartKind::OtherFretted
    } else {
        PartKind::Drums
    }
}

pub(crate) fn parse_gpif(xml: &str) -> Result<Score, String> {
    if xml.as_bytes().windows(9).any(|w| w.eq_ignore_ascii_case(b"<!DOCTYPE")) {
        return Err("score has a DOCTYPE, which is not allowed".into());
    }
    let opts = roxmltree::ParsingOptions { nodes_limit: MAX_NODES, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(xml, opts).map_err(|e| format!("score is not valid XML: {e}"))?;
    let root = doc.root_element();
    if !root.has_tag_name("GPIF") {
        return Err("not a GPIF score".into());
    }
    let (bars_by, voices_by, beats_by, notes_by, rhythms_by) = (
        by_id(root, "Bars", "Bar"),
        by_id(root, "Voices", "Voice"),
        by_id(root, "Beats", "Beat"),
        by_id(root, "Notes", "Note"),
        by_id(root, "Rhythms", "Rhythm"),
    );

    // Master bars: length, repeats, endings, sections.
    let mut masters = Vec::new();
    for mb in need(root, "MasterBars")?.children().filter(|c| c.has_tag_name("MasterBar")) {
        let time = kid_text(mb, "Time").unwrap_or("4/4");
        let (num, den) = time.split_once('/').and_then(|(n, d)| Some((n.trim().parse::<u32>().ok()?, d.trim().parse::<u32>().ok()?))).ok_or_else(|| format!("bad time signature {time:?}"))?;
        if num == 0 || den == 0 || num > 64 || den > 64 {
            return Err(format!("bad time signature {time:?}"));
        }
        let rep = kid(mb, "Repeat");
        let flag = |a: &str| rep.and_then(|r| r.attribute(a)) == Some("true");
        masters.push(Master {
            node: mb,
            len_q: num as f64 * 4.0 / den as f64,
            time_sig: (num as u8, den as u8),
            start: flag("start"),
            end: flag("end"),
            count: rep.and_then(|r| r.attribute("count")).and_then(|c| c.parse().ok()).unwrap_or(2),
            endings: ids(kid_text(mb, "AlternateEndings")).filter_map(|e| e.parse().ok()).collect(),
        });
    }
    let order = unroll(&masters)?;

    // Played bars with their start times, then the tempo automations on them.
    let mut bars = Vec::with_capacity(order.len());
    let mut start_q = 0.0;
    for &mi in &order {
        let m = &masters[mi];
        let section = kid(m.node, "Section").and_then(|s| kid_text(s, "Text").filter(|t| !t.is_empty()).or_else(|| kid_text(s, "Letter")));
        bars.push(Bar { start_q, len_q: m.len_q, time_sig: m.time_sig, tempos: Vec::new(), section: section.map(sanitize_display) });
        start_q += m.len_q;
    }
    let mut autos: Vec<(usize, f64, f64)> = Vec::new();
    if let Some(a) = kid(root, "MasterTrack").and_then(|t| kid(t, "Automations")) {
        for au in a.children().filter(|c| c.has_tag_name("Automation") && kid_text(*c, "Type") == Some("Tempo")) {
            let bar: usize = kid_text(au, "Bar").and_then(|b| b.parse().ok()).ok_or("tempo has no bar")?;
            let pos: f64 = kid_text(au, "Position").map_or(Ok(0.0), str::parse).map_err(|_| "tempo position is not a number")?;
            if !(0.0..=1.0).contains(&pos) {
                return Err(format!("tempo position {pos} is out of range"));
            }
            let mut value = kid_text(au, "Value").unwrap_or("").split_whitespace();
            let bpm: f64 = value.next().and_then(|v| v.parse().ok()).ok_or("tempo has no value")?;
            if let Some(unit) = value.next().filter(|u| *u != "2") {
                return Err(format!("tempo unit {unit} is not supported"));
            }
            if !(10.0..=400.0).contains(&bpm) {
                return Err(format!("tempo {bpm} bpm is out of range"));
            }
            autos.push((bar, pos, bpm));
        }
    }
    let mut bpm = autos.iter().find(|a| a.0 == 0).map_or(120.0, |a| a.2);
    for (b, &mi) in bars.iter_mut().zip(&order) {
        let mut here: Vec<(f64, f64)> = autos.iter().filter(|a| a.0 == mi).map(|a| (a.1 * b.len_q, a.2)).collect();
        here.sort_by(|x, y| x.0.total_cmp(&y.0));
        if here.first().is_none_or(|h| h.0 > 0.0) {
            here.insert(0, (0.0, bpm));
        }
        bpm = here.last().map_or(bpm, |h| h.1);
        b.tempos = here;
    }

    // Tracks and their notes.
    let mut parts = Vec::new();
    let mut total_notes = 0;
    let mut total_beats = 0usize;
    let mut lyrics = Vec::new();
    for (ti, track) in need(root, "Tracks")?.children().filter(|c| c.has_tag_name("Track")).enumerate() {
        if ti >= MAX_TRACKS {
            return Err(format!("too many tracks (more than {MAX_TRACKS})"));
        }
        let tuning: Vec<u8> = track
            .descendants()
            .find(|p| p.has_tag_name("Property") && p.attribute("name") == Some("Tuning"))
            .map(|p| ids(kid_text(p, "Pitches")).map(|v| v.parse::<u8>().ok().filter(|p| *p <= 127).ok_or_else(|| format!("bad tuning pitch {v:?}"))).collect::<Result<_, _>>())
            .transpose()?
            .unwrap_or_default();
        if tuning.len() > MAX_STRINGS {
            return Err(format!("more than {MAX_STRINGS} strings"));
        }
        let set = kid(track, "InstrumentSet");
        let itype = set.and_then(|s| kid_text(s, "Type")).unwrap_or("");
        let kind = instrument_kind(itype, !tuning.is_empty());
        let mut name = kid_text(track, "Name").unwrap_or("").to_string();
        if name.is_empty() || name == "Track" {
            name = set.and_then(|s| kid_text(s, "Name")).filter(|n| !n.is_empty()).unwrap_or(itype).to_string();
        }
        let mut notes = Vec::new();
        let mut words = Vec::new();
        let mut plays = vec![0usize; masters.len()];
        if kind != PartKind::Drums {
            for (b, &mi) in bars.iter().zip(&order) {
                // A repeated bar sings its next verse (line) each time round, else the first.
                let pass = plays[mi];
                plays[mi] += 1;
                let bar_id = ids(kid_text(masters[mi].node, "Bars")).nth(ti);
                let Some(bar) = bar_id.and_then(|id| bars_by.get(id)) else { continue };
                let Some(voice) = ids(kid_text(*bar, "Voices")).find(|v| *v != "-1").and_then(|v| voices_by.get(v)) else { continue };
                let mut at = 0.0;
                for beat_id in ids(kid_text(*voice, "Beats")) {
                    total_beats += 1;
                    if total_beats > MAX_BEATS {
                        return Err(format!("too many beats (more than {MAX_BEATS})"));
                    }
                    let beat = beats_by.get(beat_id).ok_or("a bar names a beat that is not in the score")?;
                    if kid(*beat, "GraceNotes").is_some() {
                        continue;
                    }
                    let rid = kid(*beat, "Rhythm").and_then(|r| r.attribute("ref")).ok_or("a beat has no rhythm")?;
                    let (len_q, dots, tuplet) = rhythm(*rhythms_by.get(rid).ok_or("a beat names a rhythm that is not in the score")?)?;
                    if let Some(word) = kid(*beat, "Lyrics").and_then(|l| beat_lyric(l, pass)) {
                        words.push((b.start_q + at, word));
                    }
                    for nid in ids(kid_text(*beat, "Notes")) {
                        let n = notes_by.get(nid).ok_or("a beat names a note that is not in the score")?;
                        let prop = |p: &str, tag: &str| property(*n, p).and_then(|p| kid_text(p, tag)).and_then(|v| v.parse::<i64>().ok());
                        let (Some(string), Some(fret)) = (prop("String", "String"), prop("Fret", "Fret")) else { continue };
                        if !(0..tuning.len() as i64).contains(&string) {
                            return Err(format!("note on string {string} but the part has {} strings", tuning.len()));
                        }
                        if !(0..=MAX_FRET).contains(&fret) {
                            return Err(format!("fret {fret} is out of range"));
                        }
                        total_notes += 1;
                        if total_notes > MAX_NOTES {
                            return Err(format!("more than {MAX_NOTES} notes"));
                        }
                        notes.push(Note {
                            q: b.start_q + at,
                            len_q,
                            string: string as u8,
                            fret: fret as u8,
                            tied: kid(*n, "Tie").and_then(|t| t.attribute("destination")) == Some("true"),
                            dots,
                            tuplet,
                            fx: effects(*n),
                        });
                    }
                    at += len_q;
                }
            }
        }
        // A bar holding more beats than its length writes past the next bar's start.
        notes.sort_by(|a, b| a.q.total_cmp(&b.q));
        if lyrics.is_empty() {
            words.sort_by(|a: &(f64, String), b| a.0.total_cmp(&b.0));
            lyrics = words;
        }
        parts.push(Part { name: sanitize_display(&name), kind, tuning, notes });
    }

    let score = need(root, "Score")?;
    Ok(Score {
        title: sanitize_display(kid_text(score, "Title").unwrap_or("")),
        artist: sanitize_display(kid_text(score, "Artist").unwrap_or("")),
        bars,
        parts,
        lyrics,
    })
}

/// A beat's syllable from its `<Lyrics>`: line `pass` (the verse sung on that
/// time round a repeat) if it has one, else the first line.
// shortcut: reads only lyrics Guitar Pro has dispatched to the beats (`<Beat><Lyrics><Line>`),
// not a track's undispatched `<Lyrics><Line><Text>`; add that if a file turns up with only the latter.
fn beat_lyric(lyrics: Node, pass: usize) -> Option<String> {
    let lines: Vec<String> = lyrics
        .children()
        .filter(|c| c.has_tag_name("Line"))
        .map(|l| sanitize_display(l.text().unwrap_or("").trim()))
        .collect();
    lines.get(pass).filter(|w| !w.is_empty()).or_else(|| lines.first().filter(|w| !w.is_empty())).cloned()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// A zip of `(name, data, deflate)` entries; CRCs are left zero (not read).
    pub(crate) fn zip_of(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let (mut out, mut dir) = (Vec::new(), Vec::new());
        for (name, data, deflate) in entries {
            let packed = if *deflate {
                let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                e.write_all(data).unwrap();
                e.finish().unwrap()
            } else {
                data.to_vec()
            };
            let method: u16 = if *deflate { 8 } else { 0 };
            let offset = out.len() as u32;
            for (head, sig, ver) in [(&mut out, LOCAL_SIG, 20u16), (&mut dir, CENTRAL_SIG, 20)] {
                head.extend(sig.to_le_bytes());
                if sig == CENTRAL_SIG {
                    head.extend(20u16.to_le_bytes()); // version made by
                }
                head.extend(ver.to_le_bytes());
                head.extend([0, 0]); // flags
                head.extend(method.to_le_bytes());
                head.extend([0; 4]); // time, date
                head.extend([0; 4]); // crc
                head.extend((packed.len() as u32).to_le_bytes());
                head.extend((data.len() as u32).to_le_bytes());
                head.extend((name.len() as u16).to_le_bytes());
                head.extend([0, 0]); // extra
                if sig == CENTRAL_SIG {
                    head.extend([0; 2 + 2 + 2 + 4]); // comment, disk, internal, external attrs
                    head.extend(offset.to_le_bytes());
                }
                head.extend(name.as_bytes());
                if sig == LOCAL_SIG {
                    head.extend(&packed);
                }
            }
        }
        let dir_at = out.len() as u32;
        out.extend(&dir);
        out.extend(EOCD_SIG.to_le_bytes());
        out.extend([0; 4]); // disk numbers
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((dir.len() as u32).to_le_bytes());
        out.extend(dir_at.to_le_bytes());
        out.extend([0, 0]); // comment length
        out
    }

    #[test]
    fn a_stored_and_a_deflated_entry_read_back() {
        let xml = b"<GPIF>".repeat(100);
        let zip = zip_of(&[("VERSION", b"7.0", false), ("Content/score.gpif", &xml, true)]);
        assert_eq!(zip_entry(&zip, "Content/score.gpif", 1 << 26), Ok(xml));
        assert_eq!(zip_entry(&zip, "VERSION", 1 << 26), Ok(b"7.0".to_vec()));
    }

    #[test]
    fn a_missing_entry_says_so() {
        let zip = zip_of(&[("VERSION", b"7.0", false)]);
        assert!(zip_entry(&zip, "Content/score.gpif", 1 << 26).unwrap_err().contains("score.gpif"));
    }

    #[test]
    fn a_truncated_zip_is_refused() {
        let zip = zip_of(&[("Content/score.gpif", &[7u8; 5000], true)]);
        assert!(zip_entry(&zip[..zip.len() / 2], "Content/score.gpif", 1 << 26).is_err());
        assert!(zip_entry(&zip[..10], "Content/score.gpif", 1 << 26).is_err());
        assert!(zip_entry(&[], "x", 1 << 26).is_err());
    }

    #[test]
    fn an_entry_larger_than_the_cap_is_refused() {
        let mut zip = zip_of(&[("Content/score.gpif", &vec![0u8; 1 << 20], true)]);
        let err = zip_entry(&zip, "Content/score.gpif", 64 << 10).unwrap_err();
        assert!(err.contains("too large"), "{err}");
        // The directory lies: it declares 10 bytes. The inflate still stops at the cap.
        let central = zip.windows(4).position(|w| w == CENTRAL_SIG.to_le_bytes()).unwrap();
        zip[central + 24..central + 28].copy_from_slice(&10u32.to_le_bytes());
        let err = zip_entry(&zip, "Content/score.gpif", 64 << 10).unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    // ---- GPIF reader ----

    /// A beat: (rhythm id, notes as (string, fret, tie destination)). Rhythms:
    /// 0 quarter, 1 dotted eighth, 2 eighth triplet, 3 half, 4 whole, 5 eighth.
    type TBeat = (usize, Vec<(i32, i32, bool)>);

    struct TTrack {
        name: &'static str,
        itype: &'static str,
        tuning: &'static str,
        bars: Vec<Vec<TBeat>>,
    }

    fn guitar(bars: Vec<Vec<TBeat>>) -> TTrack {
        TTrack { name: "Gtr", itype: "electricGuitar", tuning: "40 45 50 55 59 64", bars }
    }

    /// `masters`: (time, extra xml inside the MasterBar); `tempos`: (bar, position, value).
    fn gpif(title: &str, tracks: &[TTrack], masters: &[(&str, &str)], tempos: &[(usize, f64, &str)]) -> String {
        let (mut bars, mut voices, mut beats, mut notes) = (String::new(), String::new(), String::new(), String::new());
        let (mut next_beat, mut next_note) = (0, 0);
        let mut bar_ids: Vec<Vec<usize>> = vec![Vec::new(); masters.len()];
        let mut next_bar = 0;
        let mut tr = String::new();
        for (ti, t) in tracks.iter().enumerate() {
            let props = if t.tuning.is_empty() {
                String::new()
            } else {
                format!(r#"<Properties><Property name="Tuning"><Pitches>{}</Pitches></Property></Properties>"#, t.tuning)
            };
            tr += &format!(
                r#"<Track id="{ti}"><Name>{}</Name><InstrumentSet><Name>Electric Guitar</Name><Type>{}</Type></InstrumentSet><Staves><Staff>{props}</Staff></Staves></Track>"#,
                t.name, t.itype
            );
            for (mi, ids) in bar_ids.iter_mut().enumerate() {
                let id = next_bar;
                next_bar += 1;
                ids.push(id);
                let mut ids_beats = Vec::new();
                for (r, ns) in t.bars.get(mi).map(Vec::as_slice).unwrap_or(&[]) {
                    let mut note_ids = Vec::new();
                    for (s, f, tie) in ns {
                        notes += &format!(
                            r#"<Note id="{next_note}"><Tie origin="false" destination="{tie}"/><Properties><Property name="String"><String>{s}</String></Property><Property name="Fret"><Fret>{f}</Fret></Property></Properties></Note>"#
                        );
                        note_ids.push(next_note.to_string());
                        next_note += 1;
                    }
                    let n = if note_ids.is_empty() { String::new() } else { format!("<Notes>{}</Notes>", note_ids.join(" ")) };
                    beats += &format!(r#"<Beat id="{next_beat}"><Rhythm ref="{r}"/>{n}</Beat>"#);
                    ids_beats.push(next_beat.to_string());
                    next_beat += 1;
                }
                voices += &format!(r#"<Voice id="{id}"><Beats>{}</Beats></Voice>"#, ids_beats.join(" "));
                bars += &format!(r#"<Bar id="{id}"><Voices>{id} -1 -1 -1</Voices></Bar>"#);
            }
        }
        let mut mb = String::new();
        for (i, (time, extra)) in masters.iter().enumerate() {
            let ids: Vec<String> = bar_ids[i].iter().map(|b| b.to_string()).collect();
            mb += &format!("<MasterBar><Time>{time}</Time><Bars>{}</Bars>{extra}</MasterBar>", ids.join(" "));
        }
        let autos: String = tempos
            .iter()
            .map(|(b, p, v)| {
                format!("<Automation><Type>Tempo</Type><Linear>false</Linear><Bar>{b}</Bar><Position>{p}</Position><Visible>true</Visible><Value>{v}</Value></Automation>")
            })
            .collect();
        format!(
            r#"<?xml version="1.0"?><GPIF><Score><Title>{title}</Title><Artist>Band</Artist></Score><MasterTrack><Automations>{autos}</Automations></MasterTrack><Tracks>{tr}</Tracks><MasterBars>{mb}</MasterBars><Bars>{bars}</Bars><Voices>{voices}</Voices><Beats>{beats}</Beats><Notes>{notes}</Notes><Rhythms><Rhythm id="0"><NoteValue>Quarter</NoteValue></Rhythm><Rhythm id="1"><NoteValue>Eighth</NoteValue><AugmentationDot count="1"/></Rhythm><Rhythm id="2"><NoteValue>Eighth</NoteValue><PrimaryTuplet num="3" den="2"/></Rhythm><Rhythm id="3"><NoteValue>Half</NoteValue></Rhythm><Rhythm id="4"><NoteValue>Whole</NoteValue></Rhythm><Rhythm id="5"><NoteValue>Eighth</NoteValue></Rhythm></Rhythms></GPIF>"#
        )
    }

    fn one_note(s: i32, f: i32) -> Vec<Vec<TBeat>> {
        vec![vec![(0, vec![(s, f, false)])]]
    }

    #[test]
    fn strings_count_from_the_lowest_and_the_tuning_comes_from_the_staff() {
        let bass = TTrack { name: "Bass", itype: "electricBass", tuning: "28 33 38 43", bars: one_note(0, 3) };
        let s = parse_gpif(&gpif("T", &[bass], &[("4/4", "")], &[])).unwrap();
        let p = &s.parts[0];
        assert_eq!(p.tuning, [28, 33, 38, 43]);
        assert_eq!((p.notes[0].string, p.notes[0].fret, p.kind), (0, 3, crate::tab::PartKind::Bass));
    }

    #[test]
    fn durations_with_dots_and_triplets() {
        let beats = vec![(0, vec![(0, 1, false)]), (1, vec![(0, 2, false)]), (2, vec![(0, 3, false)]), (2, vec![])];
        let s = parse_gpif(&gpif("T", &[guitar(vec![beats])], &[("4/4", "")], &[])).unwrap();
        let n = &s.parts[0].notes;
        assert_eq!(n.len(), 3);
        assert_eq!((n[0].q, n[0].len_q), (0.0, 1.0));
        assert_eq!((n[1].q, n[1].len_q, n[1].dots), (1.0, 0.75, 1));
        assert!((n[2].q - 1.75).abs() < 1e-9 && (n[2].len_q - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(n[2].tuplet, Some((3, 2)));
    }

    #[test]
    fn tempo_and_time_signatures_place_the_bars() {
        let m = [("4/4", ""), ("4/4", ""), ("6/8", "")];
        let s = parse_gpif(&gpif("T", &[guitar(vec![])], &m, &[(0, 0.0, "85 2"), (1, 0.5, "120 2")])).unwrap();
        assert_eq!((s.bars[2].start_q, s.bars[2].len_q), (8.0, 3.0));
        assert_eq!((s.bars[0].time_sig, s.bars[2].time_sig), ((4, 4), (6, 8)), "the file's signature, not one read off the length");
        assert_eq!(s.bars[1].tempos, [(0.0, 85.0), (2.0, 120.0)]);
        assert_eq!(s.bars[2].tempos, [(0.0, 120.0)]);
    }

    /// One note per bar, fret = the bar's number, so the played order reads off the frets.
    fn order(masters: &[(&str, &str)]) -> Vec<u8> {
        let bars = (0..masters.len()).map(|i| vec![(0, vec![(0, i as i32, false)])]).collect();
        let s = parse_gpif(&gpif("T", &[guitar(bars)], masters, &[])).unwrap();
        assert_eq!(s.bars.len(), s.parts[0].notes.len());
        for (i, n) in s.parts[0].notes.iter().enumerate() {
            assert_eq!(n.q, i as f64 * 4.0);
        }
        s.parts[0].notes.iter().map(|n| n.fret).collect()
    }

    #[test]
    fn lyrics_sit_on_their_beats_and_a_repeat_sings_the_next_verse() {
        let beats = vec![vec![(0, vec![(0, 1, false)]), (0, vec![(0, 2, false)])], vec![(0, vec![(0, 3, false)])]];
        let rep = r#"<Repeat start="true" end="true" count="2"/>"#;
        let x = gpif("T", &[guitar(beats)], &[("4/4", rep), ("4/4", "")], &[]);
        let lyric = |id: usize, lines: &str| (format!(r#"<Beat id="{id}"><Rhythm ref="0"/>"#), format!(r#"<Beat id="{id}"><Lyrics>{lines}</Lyrics><Rhythm ref="0"/>"#));
        let mut x = x;
        for (id, lines) in [(0, "<Line><![CDATA[Hel-]]></Line><Line>Once</Line>"), (1, "<Line>lo</Line><Line></Line>"), (2, "<Line> world </Line>")] {
            let (from, to) = lyric(id, lines);
            assert!(x.contains(&from));
            x = x.replace(&from, &to);
        }
        let s = parse_gpif(&x).unwrap();
        let got: Vec<(f64, &str)> = s.lyrics.iter().map(|(q, w)| (*q, w.as_str())).collect();
        assert_eq!(got, [(0.0, "Hel-"), (1.0, "lo"), (4.0, "Once"), (5.0, "lo"), (8.0, "world")]);
        assert!(parse_gpif(&gpif("T", &[guitar(one_note(0, 1))], &[("4/4", "")], &[])).unwrap().lyrics.is_empty());
    }

    #[test]
    fn repeats_are_unrolled_with_their_endings() {
        let start = r#"<Repeat start="true" end="false" count="0"/>"#;
        let end = r#"<Repeat start="false" end="true" count="2"/>"#;
        let m = [
            ("4/4", ""),
            ("4/4", start),
            ("4/4", &format!("<AlternateEndings>1</AlternateEndings>{end}")[..]),
            ("4/4", "<AlternateEndings>2</AlternateEndings>"),
        ];
        assert_eq!(order(&m), [0, 1, 2, 1, 3]);
    }

    #[test]
    fn a_repeat_start_with_no_end_repeats_nothing() {
        let m = [("4/4", ""), ("4/4", r#"<Repeat start="true" end="false" count="0"/>"#), ("4/4", ""), ("4/4", "")];
        assert_eq!(order(&m), [0, 1, 2, 3]);
    }

    #[test]
    fn a_repeat_end_after_the_endings_repeats_from_past_them() {
        let start = r#"<Repeat start="true" end="false" count="0"/>"#;
        let end = r#"<Repeat start="false" end="true" count="2"/>"#;
        let m = [
            ("4/4", start),
            ("4/4", &format!("<AlternateEndings>1</AlternateEndings>{end}")[..]),
            ("4/4", "<AlternateEndings>2</AlternateEndings>"),
            ("4/4", ""),
            ("4/4", end),
        ];
        assert_eq!(order(&m), [0, 1, 0, 2, 3, 4, 3, 4]);
    }

    #[test]
    fn an_overfull_bar_sorts_its_notes_and_the_beat_and_track_limits_refuse() {
        // Six quarters in a 4/4 bar run past the next bar's first note.
        let q = |f| (0, vec![(0, f, false)]);
        let s = parse_gpif(&gpif("T", &[guitar(vec![vec![q(0), q(1), q(2), q(3), q(4), q(5)], vec![q(6)]])], &[("4/4", ""), ("4/4", "")], &[])).unwrap();
        let n = &s.parts[0].notes;
        assert!(n.windows(2).all(|w| w[0].q <= w[1].q));
        // One voice naming a note-less beat a million times over: counted, refused.
        let x = gpif("T", &[guitar(vec![vec![(0, vec![])]])], &[("4/4", "")], &[]);
        let x = x.replace("<Beats>0</Beats>", &format!("<Beats>{}</Beats>", "0 ".repeat(MAX_BEATS + 1)));
        let err = parse_gpif(&x).unwrap_err();
        assert!(err.contains("too many beats"), "{err}");
        let tracks: Vec<TTrack> = (0..65).map(|_| guitar(one_note(0, 1))).collect();
        let err = parse_gpif(&gpif("T", &tracks, &[("4/4", "")], &[])).unwrap_err();
        assert!(err.contains("too many tracks"), "{err}");
    }

    #[test]
    fn techniques_are_read_from_the_note() {
        let fx = |xml: &str| {
            let doc = roxmltree::Document::parse(xml).unwrap();
            effects(doc.root_element())
        };
        let p = |name: &str, body: &str| format!(r#"<Property name="{name}">{body}</Property>"#);
        let note = |props: &[String], kids: &str| format!("<Note><Properties>{}</Properties>{kids}</Note>", props.concat());
        assert_eq!(fx(&note(&[], "")), Effects::default());
        let e = fx(&note(
            &[p("HopoOrigin", "<Enable/>"), p("Slide", "<Flags>18</Flags>"), p("Muted", "<Enable/>"), p("PalmMuted", "<Enable/>"), p("HarmonicType", "<HType>Natural</HType>")],
            "<Vibrato>Slight</Vibrato><LetRing/>",
        ));
        assert!(e.hopo && e.vibrato && e.dead && e.palm_mute && e.let_ring && e.harmonic && !e.bend);
        assert_eq!(e.slide, 18);
        let bend = |o: &str, m: &str, d: &str| {
            let f = |v: &str| format!("<Float>{v}</Float>");
            fx(&note(&[p("Bended", "<Enable/>"), p("BendOriginValue", &f(o)), p("BendMiddleValue", &f(m)), p("BendDestinationValue", &f(d))], ""))
        };
        assert_eq!((bend("0", "50", "100").bend, bend("0", "50", "100").release), (true, false));
        assert!(bend("0", "100", "0").release, "bend and release");
        assert!(bend("100", "50", "0").release, "pre-bend release");
    }

    #[test]
    fn tied_notes_are_not_new_onsets() {
        let b = vec![vec![(0, vec![(0, 5, false)]), (0, vec![(0, 5, true)])]];
        let s = parse_gpif(&gpif("T", &[guitar(b)], &[("4/4", "")], &[])).unwrap();
        let n = &s.parts[0].notes;
        assert_eq!((n[0].tied, n[1].tied), (false, true));
    }

    #[test]
    fn parts_for_tab_put_guitar_and_bass_first() {
        let t = |name, itype, tuning| TTrack { name, itype, tuning, bars: one_note(0, 1) };
        let tracks = [
            t("Drums", "drumKit", ""),
            t("Vocal", "voice", "40 45 50 55 59 64"),
            t("Track", "electricGuitar", "40 45 50 55 59 64"),
            t("Bass", "electricBass", "28 33 38 43"),
        ];
        let s = parse_gpif(&gpif("T", &tracks, &[("4/4", "")], &[])).unwrap();
        assert_eq!(s.tab_parts(), [2, 3, 1]);
        assert_eq!(s.parts[2].name, "Electric Guitar");
        assert!(s.parts[0].notes.is_empty());
    }

    #[test]
    fn a_note_off_the_tuning_is_refused() {
        let m = [("4/4", "")];
        let err = parse_gpif(&gpif("T", &[guitar(one_note(6, 1))], &m, &[])).unwrap_err();
        assert!(err.contains("string"), "{err}");
        assert!(parse_gpif(&gpif("T", &[guitar(one_note(0, 31))], &m, &[])).is_err());
        assert!(parse_gpif(&gpif("T", &[guitar(one_note(0, 30))], &m, &[])).is_ok());
        assert!(parse_gpif(&gpif("T", &[guitar(one_note(0, 1))], &m, &[(0, 0.0, "500 2")])).is_err());
    }

    #[test]
    fn a_doctype_is_refused() {
        let xml = gpif("T", &[guitar(one_note(0, 1))], &[("4/4", "")], &[]);
        assert!(parse_gpif(&xml).is_ok());
        let with = xml.replacen("<GPIF>", "<!doctype x [<!ENTITY a \"b\">]><GPIF>", 1);
        assert!(parse_gpif(&with).unwrap_err().contains("DOCTYPE"));
    }

    #[test]
    fn malformed_scores_are_errors_not_panics() {
        assert!(parse_gpif("<GPIF>").is_err());
        assert!(parse_gpif("<GPIF/>").is_err());
        let xml = gpif("T", &[guitar(one_note(0, 1))], &[("4/4", "")], &[]);
        assert!(parse_gpif(&xml.replace("<Time>4/4</Time>", "<Time>x</Time>")).is_err());
        assert!(parse_gpif(&xml.replace(r#"<Rhythm ref="0"/>"#, r#"<Rhythm ref="9"/>"#)).is_err());
    }

    #[test]
    fn every_limit_refuses() {
        let m = [("4/4", "")];
        let err = |x: String| parse_gpif(&x).unwrap_err();
        let g = |tuning| TTrack { name: "G", itype: "electricGuitar", tuning, bars: one_note(0, 1) };
        let ok = gpif("T", &[g("40 45")], &m, &[]);
        let forever = [("4/4", r#"<Repeat start="true" end="true" count="10000"/>"#)];
        assert!(err(gpif("T", &[g("40 45")], &forever, &[])).contains("bars"));
        assert!(err(gpif("T", &[g("1 2 3 4 5 6 7 8 9 10 11")], &m, &[])).contains("strings"));
        assert!(err(gpif("T", &[g("40 128")], &m, &[])).contains("pitch"));
        assert!(err(gpif("T", &[g("40 45")], &m, &[(0, 0.0, "85 3")])).contains("tempo unit 3"));
        assert!(err(gpif("T", &[g("40 45")], &m, &[(0, 0.0, "NaN 2")])).contains("tempo"));
        assert!(err(gpif("T", &[g("40 45")], &m, &[(0, 1.5, "85 2")])).contains("position"));
        let nan = gpif("T", &[g("40 45")], &m, &[(0, 0.0, "85 2")]).replace("<Position>0</Position>", "<Position>NaN</Position>");
        assert!(err(nan).contains("position"));
        assert!(parse_gpif(&ok).is_ok());
        assert!(parse_gpif(&gpif("T", &[g("40 45")], &m, &[(0, 0.0, "85")])).is_ok());
    }

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("keet_gp_{}_{name}", std::process::id()))
    }

    #[test]
    fn read_score_reads_a_gp_file() {
        let xml = gpif("Song\u{9b}31m\u{202E}", &[guitar(one_note(0, 1))], &[("4/4", "")], &[]);
        let path = temp("ok.gp");
        std::fs::write(&path, zip_of(&[("Content/score.gpif", xml.as_bytes(), true)])).unwrap();
        let s = read_score(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(s.title.starts_with("Song") && !s.title.chars().any(|c| c.is_control() || c == '\u{202E}'), "{:?}", s.title);
    }

    #[test]
    fn a_file_over_32_mb_is_refused() {
        let path = temp("big.gp");
        std::fs::File::create(&path).unwrap().set_len(32 * 1024 * 1024 + 1).unwrap();
        let err = read_score(&path).unwrap_err();
        std::fs::remove_file(&path).ok();
        assert!(err.contains("too large"), "{err}");
    }

    /// Real-file check: `KEET_TAB_GP=/path/song.gp cargo test real_gp -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_gp() {
        let path = std::env::var("KEET_TAB_GP").expect("KEET_TAB_GP");
        let s = read_score(std::path::Path::new(&path)).unwrap();
        println!("{} / {}: {} bars, {:.0} q, tab parts {:?}", s.title, s.artist, s.bars.len(), s.len_q(), s.tab_parts());
        println!("bar 0 tempos {:?} len {}", s.bars[0].tempos, s.bars[0].len_q);
        for (i, p) in s.parts.iter().enumerate() {
            println!("{i} {} {:?} {} {:?} notes {}", p.name, p.kind, crate::tab::tuning_name(&p.tuning), p.tuning, p.notes.len());
        }
        let first = s.parts.iter().find(|p| p.kind == crate::tab::PartKind::Guitar).unwrap();
        for n in first.notes.iter().filter(|n| n.q >= 8.0).take(3) {
            println!("guitar q {} len {} string {} fret {}", n.q, n.len_q, n.string, n.fret);
        }
    }
}
