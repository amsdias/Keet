//! Score time (quarter notes) to song time (seconds): the score's tempo map,
//! stretched between the user's bar marks, plus a nudge and corrections; and
//! the per-track sync file that remembers the marks.

use crate::tab::Score;
use std::collections::HashMap;
use std::path::Path;

/// What the user has set for one track against one tab file.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TrackSync {
    pub tab_file: String,
    pub part: usize,
    pub nudge: f64,
    /// (played bar, song secs), in the order pressed (undo drops the last).
    pub marks: Vec<(usize, f64)>,
}

pub struct TimeMap {
    /// (q, tempo-map secs at q, secs per quarter from q) at every tempo change.
    segs: Vec<(f64, f64, f64)>,
    /// (q, tempo-map secs, song secs) per marked bar, by bar.
    anchors: Vec<(f64, f64, f64)>,
    nudge: f64,
    corrections: Vec<(f64, f64)>,
    len_q: f64,
}

impl TimeMap {
    pub fn new(score: &Score, sync: &TrackSync, corrections: &[(f64, f64)]) -> TimeMap {
        let mut segs: Vec<(f64, f64, f64)> = Vec::new();
        let mut t = 0.0;
        for bar in &score.bars {
            let mut tempos = bar.tempos.clone();
            if tempos.is_empty() {
                tempos.push((0.0, segs.last().map_or(120.0, |s| 60.0 / s.2)));
            }
            for (i, &(off, bpm)) in tempos.iter().enumerate() {
                let q = bar.start_q + off;
                let end = tempos.get(i + 1).map_or(bar.start_q + bar.len_q, |n| bar.start_q + n.0);
                let spq = 60.0 / bpm.max(1.0);
                segs.push((q, t, spq));
                t += (end - q).max(0.0) * spq;
            }
        }
        let mut map = TimeMap {
            segs,
            anchors: Vec::new(),
            nudge: sync.nudge,
            corrections: Vec::new(),
            len_q: score.len_q(),
        };

        // The latest press of a bar wins; song time must keep rising with the bar.
        let mut marks: Vec<(usize, f64)> = Vec::new();
        for &m in sync.marks.iter().rev().filter(|m| m.0 < score.bars.len()) {
            if !marks.iter().any(|k| k.0 == m.0) {
                marks.push(m);
            }
        }
        marks.sort_by_key(|m| m.0);
        let mut last = f64::NEG_INFINITY;
        marks.retain(|m| {
            let keep = m.1 > last;
            last = last.max(m.1);
            keep
        });
        map.anchors = marks
            .iter()
            .map(|&(bar, s)| {
                let q = score.bars[bar].start_q;
                (q, map.tempo_secs(q), s)
            })
            .collect();
        // Song time keeps rising: a correction falling to the next one (a
        // mark's 0) faster than the map runs between them is lowered to half
        // of the map's time, from the end back so a lowered one is checked too.
        let mut c = corrections.to_vec();
        c.sort_by(|a, b| a.0.total_cmp(&b.0));
        for i in (1..c.len()).rev() {
            let room = 0.5 * (map.secs(c[i].0).unwrap_or(0.0) - map.secs(c[i - 1].0).unwrap_or(0.0));
            let at_mark = map.anchors.iter().any(|a| a.0 == c[i - 1].0);
            if !at_mark && c[i - 1].1 > c[i].1 + room {
                c[i - 1].1 = c[i].1 + room;
            }
        }
        map.corrections = c;
        map
    }

    fn tempo_secs(&self, q: f64) -> f64 {
        let i = self.segs.partition_point(|s| s.0 <= q).saturating_sub(1);
        self.segs.get(i).map_or(0.0, |s| s.1 + (q - s.0) * s.2)
    }

    fn correction(&self, q: f64) -> f64 {
        let c = &self.corrections;
        let i = c.partition_point(|p| p.0 <= q);
        if c.is_empty() {
            return 0.0;
        }
        if i == 0 || i == c.len() {
            return c[i.saturating_sub(1)].1; // the ends hold
        }
        let (a, b) = (c[i - 1], c[i]);
        a.1 + (q - a.0) / (b.0 - a.0) * (b.1 - a.1)
    }

    /// Song seconds at `q`; None until a bar is marked.
    pub fn secs(&self, q: f64) -> Option<f64> {
        let a = &self.anchors;
        if a.is_empty() {
            return None;
        }
        let i = a.partition_point(|p| p.0 <= q);
        let ts = self.tempo_secs(q);
        let base = if i == 0 {
            a[0].2 + ts - a[0].1
        } else if i == a.len() {
            a[i - 1].2 + ts - a[i - 1].1
        } else {
            let (p, n) = (a[i - 1], a[i]);
            p.2 + (ts - p.1) * (n.2 - p.2) / (n.1 - p.1)
        };
        Some(base + self.nudge + self.correction(q))
    }

    /// The quarter note playing at `secs`: 0.0 before bar 1 (or unmarked), the
    /// score's end after it.
    pub fn q_at(&self, secs: f64) -> f64 {
        let Some(start) = self.secs(0.0) else { return 0.0 };
        if secs <= start {
            return 0.0;
        }
        if self.secs(self.len_q).is_none_or(|e| secs >= e) {
            return self.len_q;
        }
        let (mut lo, mut hi) = (0.0, self.len_q);
        for _ in 0..60 {
            let mid = (lo + hi) / 2.0;
            if self.secs(mid).unwrap_or(0.0) < secs {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo + hi) / 2.0
    }
}

/// Enter: mark the bar under the playhead as starting now.
pub fn mark_now(sync: &mut TrackSync, map: &TimeMap, score: &Score, now: f64) {
    let b = score.bar_at(map.q_at(now));
    let start = |i: usize| map.secs(score.bars[i].start_q).map_or(f64::MAX, |t| (t - now).abs());
    let bar = if b + 1 < score.bars.len() && start(b + 1) < start(b) { b + 1 } else { b };
    sync.marks.retain(|m| m.0 != bar);
    sync.marks.push((bar, now));
}

/// `,` / `.`: move the mark made last `by` bars (it snapped to the wrong
/// bar line where the unmarked tempo map was far off); a mark already on
/// that bar gives way. `,` on bar 1 starts bar 1 a bar later instead (the
/// first Enter always marks bar 1, so `,` could only undo a `.`). False when
/// there is no mark or no such bar.
pub fn move_mark(sync: &mut TrackSync, score: &Score, by: isize) -> bool {
    let Some(&(bar, secs)) = sync.marks.last() else { return false };
    if bar == 0 && by < 0 {
        let bar_one = TimeMap::new(score, &TrackSync { marks: vec![(0, 0.0)], ..Default::default() }, &[]);
        let Some(len) = score.bars.first().and_then(|b| bar_one.secs(b.len_q)) else { return false };
        if let Some(m) = sync.marks.last_mut() {
            m.1 += len * -by as f64;
        }
        return true;
    }
    let Some(to) = bar.checked_add_signed(by).filter(|&b| b < score.bars.len()) else { return false };
    sync.marks.pop();
    sync.marks.retain(|m| m.0 != to);
    sync.marks.push((to, secs));
    true
}

/// Backspace: drop the mark made last (press order, not the highest bar).
pub fn undo_mark(sync: &mut TrackSync) {
    sync.marks.pop();
}

/// Entries read one by one (a bad one is dropped, not the file); false when
/// the file is not a JSON object at all, so it is never saved over.
fn parse_store(text: &str) -> (HashMap<String, TrackSync>, bool) {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(o)) => {
            let map = o.into_iter().filter_map(|(k, v)| Some((k, serde_json::from_value(v).ok()?))).collect();
            (map, true)
        }
        _ => (HashMap::new(), false),
    }
}

/// Per-track sync, remembered in `tab_sync.json` (audio path → sync).
#[derive(Default)]
pub struct SyncStore {
    map: HashMap<String, TrackSync>,
    /// False when the file on disk could not be read as a store.
    readonly: bool,
}

impl SyncStore {
    fn file() -> Option<std::path::PathBuf> {
        // Tests must never read or write the user's real file.
        if cfg!(test) {
            return None;
        }
        crate::playlist::keet_config_dir().map(|d| d.join("tab_sync.json"))
    }

    pub fn load() -> Self {
        let (map, writable) = Self::file()
            .and_then(|f| crate::config::read_json_text(&f))
            .map_or((HashMap::new(), true), |t| parse_store(&t));
        SyncStore { map, readonly: !writable }
    }

    /// The saved sync for `audio`, if it was made against `tab_file`.
    pub fn get(&self, audio: &Path, tab_file: &str) -> TrackSync {
        match self.map.get(&*audio.to_string_lossy()) {
            Some(s) if s.tab_file == tab_file => s.clone(),
            _ => TrackSync::default(),
        }
    }

    pub fn set(&mut self, audio: &Path, sync: &TrackSync) {
        self.map.insert(audio.to_string_lossy().into_owned(), sync.clone());
        if let Some(f) = Self::file().filter(|_| !self.readonly) {
            if let Some(dir) = f.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(json) = serde_json::to_string_pretty(&self.map) {
                let _ = crate::playlist::write_atomic(&f, json.as_bytes());
            }
        }
    }
}

/// Beat-tracking frames per second (5 ms frames).
const FPS: f64 = 200.0;
/// Beats per tracking window, and the step between windows.
const WINDOW: usize = 16;
const WINDOW_STEP: f64 = 8.0;
/// Corrections smaller than this are dropped: a song played to a click stays
/// exactly on its tempo map.
const MIN_CORRECTION: f64 = 0.03;

/// A window leaves the last offset only for a fit this much better than
/// staying (its best tempo at that offset): sharp 5 ms onsets make a few ms
/// of a band's own looseness look like a gain, and a click-tracked song
/// (Do I Wanna Know) wandered ±0.17 s; 0.35 still moved it, 0.45 and up
/// began losing the drifting songs' fit.
const MOVE_MARGIN: f32 = 0.4;

/// How far a window's tempo may differ from the map's, as a share of a beat.
const MAX_TILT: f64 = 0.03;

/// 0, -1, 1, -2, 2 … ±n: a search from the middle, so a tie keeps the nearest.
fn outward(n: i64) -> impl Iterator<Item = i64> {
    (0..=2 * n).map(|i| if i % 2 == 0 { -(i / 2) } else { (i + 1) / 2 })
}

/// The kick drum's onsets: `mono` through 40–150 Hz (f64 biquads), an RMS
/// envelope in 5 ms frames, and each frame's rise over the last (falls are 0).
pub fn onset_curve(mono: &[f32], rate: u32) -> Vec<f32> {
    use crate::eq::{BiquadCoeffs, BiquadState};
    let r = rate as f64;
    let (hp, lp) = (BiquadCoeffs::high_pass(40.0, 0.707, r), BiquadCoeffs::low_pass(150.0, 0.707, r));
    let (mut h, mut l) = (BiquadState::new(), BiquadState::new());
    let frames = (mono.len() as f64 * FPS / r) as usize;
    let (mut out, mut start, mut last) = (Vec::with_capacity(frames), 0, 0.0);
    for f in 1..=frames {
        // Frame edges from the frame number, so a rate that is not a multiple
        // of 200 does not drift.
        let end = ((f as f64 * r / FPS) as usize).min(mono.len());
        let sum: f64 = mono[start..end].iter().map(|&x| l.process(&lp, h.process(&hp, x as f64)).powi(2)).sum();
        let rms = (sum / (end - start).max(1) as f64).sqrt();
        out.push((rms - last).max(0.0) as f32);
        (start, last) = (end, rms);
    }
    out
}

/// Slide the score's beat grid onto the recording's onsets: corrections
/// (q, secs) for `TimeMap::new`. `map` must be built from the marks alone
/// (no corrections, no nudge). From every mark, windows of 16 quarter-note
/// beats, 8 apart, each slid within half a beat of the last window's offset
/// (5 ms steps), and stretched within 3% to its own tempo, to where the
/// onsets are strongest; the offset is the correction at the window's first
/// beat. A mark is never moved: the
/// correction there is 0 and the tracker starts again from it.
pub fn track_beats(map: &TimeMap, score: &Score, onsets: &[f32]) -> Vec<(f64, f64)> {
    let onset = |f: usize| onsets.get(f).copied().unwrap_or(0.0);
    // A beat's score: its frame's onset and half of each neighbour's.
    let at = |t: f64| {
        let f = (t * FPS).floor();
        if f < 0.0 {
            return 0.0;
        }
        let f = f as usize;
        onset(f) + 0.5 * (onset(f.wrapping_sub(1)) + onset(f + 1))
    };
    let len = score.len_q();
    // (q, delta, at a mark)
    let mut found: Vec<(f64, f64, bool)> = Vec::new();
    for (k, a) in map.anchors.iter().enumerate() {
        let end = map.anchors.get(k + 1).map_or(len, |n| n.0);
        found.push((a.0, 0.0, true));
        let (mut q0, mut prev) = (a.0, 0.0);
        loop {
            let beats: Vec<f64> =
                (0..WINDOW).map(|i| q0 + i as f64).filter(|&q| q < end).filter_map(|q| map.secs(q)).collect();
            if beats.len() < WINDOW / 2 {
                break;
            }
            let span = (beats.len() - 1) as f64;
            let beat = (beats[beats.len() - 1] - beats[0]) / span;
            let reach = (beat / 2.0 * FPS) as i64;
            // The window's own tempo too, in steps moving its last beat by a
            // frame: a 1% drift spreads 16 beats over 0.1 s, and slid as one
            // they score a plateau, not a peak, whose top lies anywhere in it.
            let tilt = (MAX_TILT * beat * span * FPS) as i64;
            let (mut best, mut best_score, mut stay) = (prev, f32::NEG_INFINITY, f32::NEG_INFINITY);
            for k in outward(tilt) {
                let per_beat = k as f64 / (FPS * span);
                for d in outward(reach) {
                    let off = prev + d as f64 / FPS;
                    let sc: f32 = beats.iter().enumerate().map(|(i, &t)| at(t + off + per_beat * i as f64)).sum();
                    if sc > best_score {
                        (best, best_score) = (off, sc);
                    }
                    if d == 0 {
                        stay = stay.max(sc);
                    }
                }
            }
            if best_score <= stay * (1.0 + MOVE_MARGIN) {
                best = prev;
            }
            if q0 > a.0 {
                found.push((q0, best, false));
            }
            prev = best;
            q0 += WINDOW_STEP;
        }
    }
    settle(found)
}

/// The tracker's findings, (q, delta, at a mark), as corrections. A small
/// one becomes 0 rather than going: dropped, the line between its
/// neighbours would stretch over it. None at all when every one is 0.
fn settle(found: Vec<(f64, f64, bool)>) -> Vec<(f64, f64)> {
    let c: Vec<(f64, f64)> =
        found.into_iter().map(|c| (c.0, if c.2 || c.1.abs() < MIN_CORRECTION { 0.0 } else { c.1 })).collect();
    if c.iter().all(|c| c.1 == 0.0) {
        return Vec::new();
    }
    c
}

/// The track as mono, for `onset_curve`: decoded with symphonia (as
/// playback does), at most `max_secs` of it, channels averaged, and averaged
/// down to about 2 kHz (by a whole factor of the rate, so time stays exact;
/// the kick band is far below it) — a 30-minute 192 kHz file would otherwise
/// be over a gigabyte.
pub fn decode_mono(path: &Path, max_secs: f64) -> Result<(Vec<f32>, u32), String> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::common::Limit;
    use symphonia::core::meta::MetadataOptions;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let meta = MetadataOptions::default().limit_visual_bytes(Limit::Maximum(0));
    let mut format =
        symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), meta).map_err(|e| e.to_string())?;
    let track = format.default_track(TrackType::Audio).ok_or("no audio track")?;
    let id = track.id;
    let params = track.codec_params.as_ref().and_then(|c| c.audio()).ok_or("no audio parameters")?.clone();
    let rate = params.sample_rate.ok_or("no sample rate")?;
    let mut dec = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| e.to_string())?;
    // AAC's priming is not song time (playback drops it): kicks would read late.
    let revisions = crate::metadata::revisions_newest_first(format.as_mut());
    let mut priming = crate::gapless::for_mp4(path, &revisions, rate, Some(id)).map_or(0, |g| g.delay as usize);
    let factor = (1..=(rate / 2000).max(1)).rev().find(|k| rate % k == 0).unwrap_or(1) as usize;
    let limit = (max_secs * rate as f64) as usize / factor;
    let (mut out, mut tmp, mut acc, mut n) = (Vec::new(), Vec::new(), 0.0f32, 0);
    while out.len() < limit {
        let pkt = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) if out.is_empty() => return Err(e.to_string()),
            Err(_) => break,
        };
        if pkt.track_id != id {
            continue;
        }
        // A damaged packet costs its few ms, as in playback.
        let Ok(buf) = dec.decode(&pkt) else { continue };
        let ch = buf.spec().channels().count().max(1);
        tmp.resize(buf.samples_interleaved(), 0.0f32);
        buf.copy_to_slice_interleaved(&mut tmp);
        let skip = priming.min(tmp.len() / ch);
        priming -= skip;
        for frame in tmp[skip * ch..].chunks_exact(ch) {
            acc += frame.iter().sum::<f32>() / ch as f32;
            n += 1;
            if n == factor {
                out.push(acc / factor as f32);
                (acc, n) = (0.0, 0);
            }
        }
    }
    out.truncate(limit);
    if out.is_empty() {
        return Err("no audio decoded".into());
    }
    Ok((out, rate / factor as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tab::Bar;

    fn score(bars: usize) -> Score {
        Score {
            title: String::new(),
            artist: String::new(),
            bars: (0..bars)
                .map(|i| Bar { start_q: 4.0 * i as f64, len_q: 4.0, time_sig: (4, 4), tempos: vec![(0.0, 120.0)], section: None })
                .collect(),
            parts: vec![],
            lyrics: vec![],
        }
    }

    fn sync(marks: &[(usize, f64)], nudge: f64) -> TrackSync {
        TrackSync { tab_file: "a.gp".into(), part: 0, nudge, marks: marks.to_vec() }
    }

    #[test]
    fn the_last_mark_moves_a_bar_and_takes_the_place_of_one_there() {
        let s = score(4);
        let mut sync = TrackSync { marks: vec![(2, 9.0), (0, 1.0)], ..Default::default() };
        assert!(move_mark(&mut sync, &s, 1));
        assert_eq!(sync.marks, vec![(2, 9.0), (1, 1.0)], "same time, the next bar");
        assert!(move_mark(&mut sync, &s, 1));
        assert_eq!(sync.marks, vec![(2, 1.0)], "the old mark on bar 3 gives way");
        assert!(!move_mark(&mut sync, &s, 2), "no bar past the last");
        assert!(!move_mark(&mut TrackSync::default(), &s, 1), "nothing to move");
    }

    #[test]
    fn comma_on_bar_one_starts_it_a_bar_later() {
        let s = score(8);
        let mut y = sync(&[(1, 5.0)], 0.0);
        assert!(move_mark(&mut y, &s, -1));
        assert_eq!(y.marks, vec![(0, 5.0)]);
        let bar = TimeMap::new(&s, &sync(&[(0, 0.0)], 0.0), &[]).secs(4.0).unwrap();
        assert!(move_mark(&mut y, &s, -1) && move_mark(&mut y, &s, -1));
        assert_eq!(y.marks, vec![(0, 5.0 + 2.0 * bar)]);
        assert_eq!(TimeMap::new(&s, &y, &[]).q_at(5.0 + bar), 0.0, "the tab waits on bar 1");
    }

    #[test]
    fn comma_moves_the_tab_back_as_far_as_dot_moves_it_on() {
        let s = score(60);
        let mut sync = sync(&[(23, 0.0)], 38.55);
        let q = |sync: &TrackSync| TimeMap::new(&s, sync, &[]).q_at(60.0);
        let mut last = q(&sync);
        for _ in 0..20 {
            assert!(move_mark(&mut sync, &s, -1));
            let now = q(&sync);
            assert!((last - now - 4.0).abs() < 1e-6, "{last} -> {now}");
            last = now;
        }
    }

    #[test]
    fn bar_one_alone_follows_the_tempo_map() {
        let s = score(8);
        let m = TimeMap::new(&s, &sync(&[(0, 10.0)], 0.0), &[]);
        assert_eq!(m.secs(4.0), Some(12.0));
        assert!((m.q_at(12.0) - 4.0).abs() < 1e-6);
    }

    #[test]
    fn two_marks_stretch_the_bars_between() {
        let s = score(8);
        let m = TimeMap::new(&s, &sync(&[(0, 10.0), (2, 15.0)], 0.0), &[]);
        assert_eq!(m.secs(8.0), Some(15.0));
        assert_eq!(m.secs(4.0), Some(12.5));
        assert_eq!(m.secs(12.0), Some(17.0));
    }

    #[test]
    fn a_nudge_moves_everything() {
        let s = score(8);
        let m = TimeMap::new(&s, &sync(&[(0, 10.0), (2, 15.0)], 0.05), &[]);
        assert!((m.secs(4.0).unwrap() - 12.55).abs() < 1e-9);
        assert!((m.secs(12.0).unwrap() - 17.05).abs() < 1e-9);
    }

    #[test]
    fn before_bar_one_the_tab_waits_and_after_the_end_it_stays() {
        let s = score(8);
        let m = TimeMap::new(&s, &sync(&[(0, 10.0)], 0.0), &[]);
        assert_eq!(m.q_at(5.0), 0.0);
        assert_eq!(m.q_at(1e6), s.len_q());
        let none = TimeMap::new(&s, &sync(&[], 0.0), &[]);
        assert_eq!(none.secs(4.0), None);
        assert_eq!(none.q_at(50.0), 0.0);
    }

    #[test]
    fn enter_marks_the_bar_under_the_playhead() {
        let s = score(8);
        let mut y = sync(&[], 0.0);
        let m = TimeMap::new(&s, &y, &[]);
        mark_now(&mut y, &m, &s, 3.2);
        assert_eq!(y.marks, vec![(0, 3.2)]);
        // Bars are 2 s; bar 4 starts at 11.2, so 11.5 is nearest to it.
        let m = TimeMap::new(&s, &y, &[]);
        mark_now(&mut y, &m, &s, 11.5);
        assert_eq!(y.marks.len(), 2);
        assert!(y.marks.contains(&(0, 3.2)) && y.marks.contains(&(4, 11.5)));
        let m = TimeMap::new(&s, &y, &[]);
        assert_eq!(m.secs(16.0), Some(11.5));
        // A second Enter in the same bar replaces its mark.
        mark_now(&mut y, &m, &s, 11.6);
        assert_eq!(y.marks.iter().filter(|m| m.0 == 4).collect::<Vec<_>>(), vec![&(4, 11.6)]);
        assert_eq!(y.marks.len(), 2);
    }

    #[test]
    fn a_later_enter_pins_that_bar() {
        let s = score(8);
        let mut y = sync(&[(0, 0.0)], 0.0);
        let m = TimeMap::new(&s, &y, &[]);
        mark_now(&mut y, &m, &s, 8.4);
        assert_eq!(y.marks, vec![(0, 0.0), (4, 8.4)]);
        let m = TimeMap::new(&s, &y, &[]);
        assert!((m.secs(16.0).unwrap() - 8.4).abs() < 1e-9);
    }

    #[test]
    fn backspace_drops_the_last_mark_made() {
        let mut y = sync(&[(0, 0.0), (4, 8.4), (2, 4.1)], 0.0);
        undo_mark(&mut y);
        assert_eq!(y.marks, vec![(0, 0.0), (4, 8.4)]);
        undo_mark(&mut y);
        undo_mark(&mut y);
        undo_mark(&mut y);
        assert!(y.marks.is_empty());
    }

    #[test]
    fn corrections_are_interpolated_and_zero_outside() {
        let s = score(8);
        let m = TimeMap::new(&s, &sync(&[(0, 10.0)], 0.0), &[(4.0, 0.2), (8.0, 0.0)]);
        assert!((m.secs(6.0).unwrap() - 13.1).abs() < 1e-9);
        assert_eq!(m.secs(12.0), Some(16.0));
    }

    #[test]
    fn marks_saved_for_another_tab_file_are_not_used() {
        let mut st = SyncStore::default();
        st.set(Path::new("/m/x.flac"), &sync(&[(0, 1.0)], 0.0));
        assert_eq!(st.get(Path::new("/m/x.flac"), "b.gp"), TrackSync::default());
    }

    #[test]
    fn the_store_round_trips_without_touching_the_real_file() {
        assert!(SyncStore::file().is_none());
        let mut st = SyncStore::default();
        let y = sync(&[(0, 1.0), (3, 9.5)], 0.1);
        st.set(Path::new("/m/x.flac"), &y);
        assert_eq!(st.get(Path::new("/m/x.flac"), "a.gp"), y);
    }

    #[test]
    fn a_damaged_store_keeps_the_good_entries_and_is_never_overwritten() {
        let good = serde_json::to_string(&sync(&[(0, 1.0)], 0.0)).unwrap();
        let (m, w) = parse_store(&format!(r#"{{"/a": {good}, "/b": {{"nudge": "x"}}}}"#));
        assert!(w && m.len() == 1 && m.contains_key("/a"));
        let (m, w) = parse_store("not json");
        assert!(m.is_empty() && !w);
    }

    #[test]
    fn corrections_hold_their_end_values() {
        let s = score(8);
        let m = TimeMap::new(&s, &sync(&[(0, 10.0)], 0.0), &[(4.0, 0.1), (8.0, 0.3)]);
        assert_eq!(m.secs(1.0), Some(10.0 + 0.5 + 0.1));
        assert!((m.secs(8.0).unwrap() - (14.0 + 0.3)).abs() < 1e-9);
        let past = m.secs(8.0 + 1e-6).unwrap();
        assert!((past - (m.secs(8.0).unwrap() + 0.5e-6)).abs() < 1e-9);
        assert!((m.secs(12.0).unwrap() - (16.0 + 0.3)).abs() < 1e-9);
        let mut prev = f64::MIN;
        for i in 0..=1600 {
            let v = m.secs(i as f64 * 0.01).unwrap();
            assert!(v > prev, "not increasing at {i}");
            prev = v;
        }
    }

    #[test]
    fn enter_marks_the_nearest_bar_start() {
        let s = score(8);
        let mut y = sync(&[(0, 0.0)], 0.0);
        let m = TimeMap::new(&s, &y, &[]);
        mark_now(&mut y, &m, &s, 7.9);
        assert!(y.marks.contains(&(4, 7.9)));
        // The map has bar 4 at 8.0 and bar 5 at 10.0: 8.9 is nearer the first.
        let mut y = sync(&[(0, 0.0)], 0.0);
        let m = TimeMap::new(&s, &y, &[]);
        mark_now(&mut y, &m, &s, 8.9);
        assert!(y.marks.contains(&(4, 8.9)) && !y.marks.iter().any(|m| m.0 == 5));
        // Pressing in the same bar again replaces its mark.
        let m = TimeMap::new(&s, &y, &[]);
        mark_now(&mut y, &m, &s, 8.5);
        assert_eq!(y.marks.iter().filter(|m| m.0 == 4).collect::<Vec<_>>(), vec![&(4, 8.5)]);
    }

    #[test]
    fn marks_that_go_backwards_are_dropped_and_the_latest_duplicate_wins() {
        let s = score(8);
        let m = TimeMap::new(&s, &sync(&[(0, 10.0), (2, 8.0), (3, 20.0)], 0.0), &[]);
        // (2, 8.0) goes backwards and is dropped: bar 2 sits on the 0..3 stretch.
        assert!((m.secs(8.0).unwrap() - (10.0 + 4.0 * 10.0 / 6.0)).abs() < 1e-9);
        assert_eq!(m.secs(12.0), Some(20.0));
        let m = TimeMap::new(&s, &sync(&[(0, 10.0), (2, 15.0), (2, 16.0)], 0.0), &[]);
        assert_eq!(m.secs(8.0), Some(16.0));
    }

    /// 4/4 bars at `bpm`.
    fn score_at(bars: usize, bpm: f64) -> Score {
        let mut s = score(bars);
        for b in &mut s.bars {
            b.tempos = vec![(0.0, bpm)];
        }
        s
    }

    /// `secs` of 80 Hz bursts (8 kHz), one every beat at `bpm` from `first`.
    fn kicks(secs: f64, bpm: f64, first: f64) -> Vec<f32> {
        let rate = 8000.0;
        let mut out = vec![0.0f32; (secs * rate) as usize];
        let mut t = first;
        while t < secs {
            let start = (t * rate) as usize;
            for i in 0..(0.3 * rate) as usize {
                let Some(x) = out.get_mut(start + i) else { break };
                let tt = i as f64 / rate;
                *x += (0.8 * (2.0 * std::f64::consts::PI * 80.0 * tt).sin() * (-tt / 0.05).exp()) as f32;
            }
            t += 60.0 / bpm;
        }
        out
    }

    #[test]
    fn a_steady_kick_track_needs_no_correction() {
        let s = score_at(25, 100.0);
        let onsets = onset_curve(&kicks(60.0, 100.0, 1.0), 8000);
        assert!((onsets.len() as i64 - 12_000).abs() <= 1, "200 frames a second: {}", onsets.len());
        let map = TimeMap::new(&s, &sync(&[(0, 1.0)], 0.0), &[]);
        assert_eq!(track_beats(&map, &s, &onsets), vec![]);
    }

    #[test]
    fn a_drifting_kick_track_is_followed() {
        let s = score_at(25, 100.0);
        let onsets = onset_curve(&kicks(60.0, 99.0, 1.0), 8000);
        let y = sync(&[(0, 1.0)], 0.0);
        let map = TimeMap::new(&s, &y, &[]);
        let c = track_beats(&map, &s, &onsets);
        let fixed = TimeMap::new(&s, &y, &c);
        let delta = fixed.secs(80.0).unwrap() - map.secs(80.0).unwrap();
        assert!((delta - 80.0 * (60.0 / 99.0 - 0.6)).abs() < 0.02, "correction at q 80: {delta:.3} ({c:?})");
        let burst = 1.0 + 80.0 * 60.0 / 99.0;
        assert!((fixed.secs(80.0).unwrap() - burst).abs() < 0.02);
        // The mark is never moved, nor a later one, and the tracker starts
        // again from it.
        assert!((fixed.secs(0.0).unwrap() - 1.0).abs() < 1e-9);
        let y = sync(&[(0, 1.0), (10, 1.0 + 40.0 * 60.0 / 99.0)], 0.0);
        let map = TimeMap::new(&s, &y, &[]);
        let fixed = TimeMap::new(&s, &y, &track_beats(&map, &s, &onsets));
        assert!((fixed.secs(40.0).unwrap() - map.secs(40.0).unwrap()).abs() < 1e-9);
        assert!((fixed.secs(80.0).unwrap() - burst).abs() < 0.02);
    }

    #[test]
    fn a_steady_kick_track_with_jitter_needs_no_correction() {
        let s = score_at(25, 100.0);
        let rate = 8000.0;
        let mut seed = 0x2545_f491_u64;
        let mut out = vec![0.0f32; (60.0 * rate) as usize];
        for n in 0..99 {
            // ±10 ms, seeded (a 64-bit LCG).
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let jitter = ((seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 0.02;
            let start = ((1.0 + n as f64 * 0.6 + jitter) * rate) as usize;
            for i in 0..(0.3 * rate) as usize {
                let Some(x) = out.get_mut(start + i) else { break };
                let tt = i as f64 / rate;
                *x += (0.8 * (2.0 * std::f64::consts::PI * 80.0 * tt).sin() * (-tt / 0.05).exp()) as f32;
            }
        }
        let map = TimeMap::new(&s, &sync(&[(0, 1.0)], 0.0), &[]);
        assert_eq!(track_beats(&map, &s, &onset_curve(&out, 8000)), vec![]);
    }

    #[test]
    fn secs_keeps_rising_into_a_later_mark() {
        let s = score_at(25, 100.0);
        // Bar 10 marked where the map puts it; a big correction two beats before.
        let m = TimeMap::new(&s, &sync(&[(0, 1.0), (10, 25.0)], 0.0), &[(0.0, 0.0), (38.0, 2.0), (40.0, 0.0)]);
        let mut prev = f64::MIN;
        for i in 0..=2000 {
            let v = m.secs(30.0 + i as f64 * 0.01).unwrap();
            assert!(v > prev, "not increasing at q {}", 30.0 + i as f64 * 0.01);
            prev = v;
        }
        assert!((m.secs(40.0).unwrap() - 25.0).abs() < 1e-9, "the mark stays");
    }

    #[test]
    fn small_corrections_hold_zero_between_their_neighbours() {
        let c = settle(vec![(0.0, 0.0, true), (8.0, 0.02, false), (16.0, 0.025, false), (24.0, 0.3, false)]);
        let s = score(8);
        let m = TimeMap::new(&s, &sync(&[(0, 0.0)], 0.0), &c);
        let base = TimeMap::new(&s, &sync(&[(0, 0.0)], 0.0), &[]);
        assert!(m.secs(8.0).unwrap() - base.secs(8.0).unwrap() <= 0.03, "{c:?}");
        assert!((m.secs(24.0).unwrap() - base.secs(24.0).unwrap() - 0.3).abs() < 1e-9);
    }

    #[test]
    fn decoding_drops_aac_priming_and_averages_down_to_about_2_khz() {
        let f = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sine_aac_editlist.m4a");
        let (mono, rate) = decode_mono(&f, 60.0).unwrap();
        assert_eq!(rate, 2100, "44.1 kHz / 21");
        // 1 s of sine after 1024 priming frames (and some padding): with the
        // priming the first ~49 ms would be the encoder's silence.
        assert!((2100..2150).contains(&mono.len()), "{}", mono.len());
        assert!(mono[..20].iter().any(|x| x.abs() > 0.05), "the sine from the start");
        assert!(decode_mono(&f, 0.5).unwrap().0.len() <= 1050, "max_secs caps it");
        assert!(decode_mono(Path::new("/no/such.flac"), 60.0).is_err());
    }

    /// A real score against its recording: `KEET_TAB_GP=<file.gp>
    /// KEET_TAB_AUDIO=<audio> KEET_TAB_BAR1=<secs> cargo test --release --
    /// --ignored real_music --nocapture`. Prints the first strong kicks (where
    /// bar 1 may be), how well the grid sits on the kicks before and after
    /// tracking, and the tracker's drift every 16 bars (`KEET_TAB_ALL=1`: every
    /// correction too).
    #[test]
    #[ignore]
    fn real_music() {
        let (Ok(gp), Ok(audio), Ok(bar1)) =
            (std::env::var("KEET_TAB_GP"), std::env::var("KEET_TAB_AUDIO"), std::env::var("KEET_TAB_BAR1"))
        else {
            return;
        };
        let s = crate::gp::read_score(Path::new(&gp)).unwrap();
        let t0 = std::time::Instant::now();
        let (mono, rate) = decode_mono(Path::new(&audio), 30.0 * 60.0).unwrap();
        let decoded = t0.elapsed();
        let onsets = onset_curve(&mono, rate);
        let y = sync(&[(0, bar1.parse().unwrap())], 0.0);
        let map = TimeMap::new(&s, &y, &[]);
        let c = track_beats(&map, &s, &onsets);
        let total = t0.elapsed();
        let fixed = TimeMap::new(&s, &y, &c);
        let secs = mono.len() / rate as usize;
        println!("{secs} s at {rate} Hz; decode {decoded:.2?}, decode + track {total:.2?}; {} corrections", c.len());
        let top = onsets.iter().copied().fold(0.0f32, f32::max);
        let kicks: Vec<String> =
            (0..onsets.len()).filter(|&f| onsets[f] > 0.3 * top).take(8).map(|f| format!("{:.2}", f as f64 / FPS)).collect();
        println!("first strong kicks (s): {}", kicks.join(" "));
        let fit = |m: &TimeMap| -> f32 {
            (0..s.len_q() as usize).filter_map(|q| m.secs(q as f64)).map(|t| onsets.get((t * FPS) as usize).copied().unwrap_or(0.0)).sum()
        };
        println!("onsets on the beats: map {:.3}, tracked {:.3}", fit(&map), fit(&fixed));
        if std::env::var("KEET_TAB_ALL").is_ok() {
            println!("corrections: {c:.3?}");
        }
        for b in (0..s.bars.len()).step_by(16) {
            let q = s.bars[b].start_q;
            let (m, f) = (map.secs(q).unwrap(), fixed.secs(q).unwrap());
            println!("bar {:4}  q {q:7.1}  map {m:7.2} s  tracked {f:7.2} s  drift {:+.2} s", b + 1, f - m);
        }
    }
}
