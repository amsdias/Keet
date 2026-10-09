//! Encoder delay and padding for AAC in MP4 (M4A/M4B).
//!
//! An AAC decoder emits "priming" samples before the real audio (2112 for
//! Apple's encoder, 1024 for ffmpeg's) and pads the last frame out to a whole
//! 1024. Played as decoded, every track starts ~25-50 ms late and ends with
//! up to 23 ms of silence — an audible gap between the tracks of a live album.
//! The container records the real extent in one of two ways, and symphonia
//! applies neither: its MP4 reader never sets the packets' trim values.
//!
//! - iTunSMPB, Apple's tag: hex fields " 00000000 DELAY PADDING LENGTH …"
//! - an edit list (`moov/trak/edts/elst`), which ffmpeg writes: the media time
//!   where playback starts, and how long it runs.

/// The real audio inside the decoded stream, in frames at the track's rate:
/// skip `delay` frames, then play `length` (when known). `padding` is the
/// encoder's trailing fill, when the tag says (iTunSMPB does; 0 otherwise).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gapless {
    pub delay: u64,
    pub length: Option<u64>,
    pub padding: u64,
}

impl Gapless {
    /// With no recorded length, work it out from the container's frame count:
    /// what is left after the priming and the padding. An iTunSMPB with a
    /// length field of 0 left the priming inside the track's duration.
    pub fn with_length_from(self, container_frames: u64) -> Self {
        let derived = (container_frames > 0)
            .then(|| container_frames.checked_sub(self.delay + self.padding))
            .flatten()
            .filter(|&l| l > 0);
        Self { length: self.length.or(derived), ..self }
    }
}

/// Encoder delay and padding for an MP4 file, in frames at `sample_rate`:
/// Apple's iTunSMPB tag first (already in frames), then the edit list
/// (converted from the track's media timescale). None for any other
/// container. The two came back in different units and were both converted
/// from the timescale, which trimmed iTunSMPB files wrongly whenever the
/// timescale was not the sample rate.
pub fn for_mp4(
    path: &std::path::Path,
    revisions: &[symphonia::core::meta::MetadataRevision],
    sample_rate: u32,
    track_id: Option<u32>,
) -> Option<Gapless> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    if !matches!(ext.as_str(), "m4a" | "m4b" | "mp4" | "m4p") {
        return None;
    }
    revisions
        .iter()
        .flat_map(|r| &r.media.tags)
        .find(|t| t.raw.key.to_ascii_lowercase().ends_with("itunsmpb"))
        .and_then(|t| match &t.raw.value {
            symphonia::core::meta::RawValue::String(s) => from_itunsmpb(s),
            _ => None,
        })
        .or_else(|| from_mp4_edit_list(path, sample_rate, track_id))
}

/// Parse an iTunSMPB value. None for a malformed one or one saying nothing.
pub fn from_itunsmpb(value: &str) -> Option<Gapless> {
    let fields: Vec<u64> = value
        .split_whitespace()
        .take(4)
        .map(|f| u64::from_str_radix(f, 16).ok())
        .collect::<Option<_>>()?;
    let [_, delay, padding, length] = fields[..] else { return None };
    if delay == 0 && length == 0 && padding == 0 {
        return None;
    }
    Some(Gapless { delay, length: (length > 0).then_some(length), padding })
}

/// Read the first audio track's edit list from an MP4 file, in frames at
/// `sample_rate` (the edit list counts in the track's media timescale, which
/// is usually but not always the sample rate). Only the common single-segment
/// form is used: an empty edit (a leading gap) or several segments are left
/// alone rather than half-applied. `track_id` (symphonia's, which is the
/// `tkhd` track ID) picks the track being played; None takes the first audio
/// track — it always did, so a file whose played track was not the first got
/// another track's trim.
pub fn from_mp4_edit_list(path: &std::path::Path, sample_rate: u32, track_id: Option<u32>) -> Option<Gapless> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let moov = find(&mut f, 0, len, b"moov")?;
    let mvhd = find(&mut f, moov.0, moov.1, b"mvhd")?;
    let movie_ts = timescale(&mut f, mvhd)?;
    for trak in children(&mut f, moov.0, moov.1).into_iter().filter(|a| &a.0 == b"trak") {
        let (start, end) = (trak.1, trak.2);
        let Some(mdia) = find(&mut f, start, end, b"mdia") else { continue };
        let Some(hdlr) = find(&mut f, mdia.0, mdia.1, b"hdlr") else { continue };
        if read_at(&mut f, hdlr.0 + 8, 4)? != b"soun" {
            continue;
        }
        if let Some(wanted) = track_id {
            // tkhd: version, flags, then two timestamps (4 bytes each in
            // version 0, 8 in version 1), then the track ID.
            let Some(tkhd) = find(&mut f, start, end, b"tkhd") else { continue };
            let version = read_at(&mut f, tkhd.0, 1)?[0];
            let at = tkhd.0 + if version == 1 { 20 } else { 12 };
            if u32::from_be_bytes(read_at(&mut f, at, 4)?.try_into().ok()?) != wanted {
                continue;
            }
        }
        let mdhd = find(&mut f, mdia.0, mdia.1, b"mdhd")?;
        let media_ts = timescale(&mut f, mdhd)?;
        // No edit list on THIS track ends the search on purpose (`?`, not
        // `continue`): this is the played track (or, with no id, the first
        // audio track — the one played), and another track's edit list is
        // not its trim. That mix-up is the bug the track-ID match fixed.
        let edts = find(&mut f, start, end, b"edts")?;
        let elst = find(&mut f, edts.0, edts.1, b"elst")?;
        let head = read_at(&mut f, elst.0, 8)?;
        let (version, count) = (head[0], u32::from_be_bytes(head[4..8].try_into().ok()?));
        if count != 1 {
            return None;
        }
        let (segment, media_time) = if version == 1 {
            let e = read_at(&mut f, elst.0 + 8, 16)?;
            (u64::from_be_bytes(e[..8].try_into().ok()?), i64::from_be_bytes(e[8..].try_into().ok()?))
        } else {
            let e = read_at(&mut f, elst.0 + 8, 8)?;
            (u32::from_be_bytes(e[..4].try_into().ok()?) as u64, i32::from_be_bytes(e[4..].try_into().ok()?) as i64)
        };
        if media_time < 0 || movie_ts == 0 {
            return None; // an empty edit: a gap before the audio, not a trim
        }
        // The segment's duration is in the MOVIE's timescale, the start in
        // the media's; both become frames at the sample rate.
        if media_ts == 0 {
            return None;
        }
        let frames = |units: u128, ts: u32| (units * sample_rate as u128 / ts as u128) as u64;
        let length = frames(segment as u128, movie_ts);
        return Some(Gapless { delay: frames(media_time as u128, media_ts), length: (length > 0).then_some(length), padding: 0 });
    }
    None
}

/// (body start, body end) of the first `kind` atom among the children of the
/// byte range.
fn find(f: &mut std::fs::File, start: u64, end: u64, kind: &[u8; 4]) -> Option<(u64, u64)> {
    children(f, start, end).into_iter().find(|a| &a.0 == kind).map(|a| (a.1, a.2))
}

/// The atoms directly inside a byte range: (type, body start, body end).
fn children(f: &mut std::fs::File, start: u64, end: u64) -> Vec<([u8; 4], u64, u64)> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos + 8 <= end {
        let Some(h) = read_at(f, pos, 8) else { break };
        let kind: [u8; 4] = h[4..8].try_into().unwrap_or_default();
        let (size, header) = match u32::from_be_bytes(h[..4].try_into().unwrap_or_default()) {
            0 => (end - pos, 8),
            1 => match read_at(f, pos + 8, 8) {
                Some(b) => (u64::from_be_bytes(b[..].try_into().unwrap_or_default()), 16),
                None => break,
            },
            n => (n as u64, 8),
        };
        if size < header || pos + size > end {
            break; // corrupt: stop rather than wander
        }
        out.push((kind, pos + header, pos + size));
        pos += size;
    }
    out
}

/// The timescale field of an mvhd/mdhd body (after version, flags and the
/// creation/modification times, which are 32- or 64-bit by version).
fn timescale(f: &mut std::fs::File, body: (u64, u64)) -> Option<u32> {
    let version = read_at(f, body.0, 1)?[0];
    let at = if version == 1 { body.0 + 20 } else { body.0 + 12 };
    let b = read_at(f, at, 4)?;
    Some(u32::from_be_bytes(b[..].try_into().ok()?))
}

/// `n` bytes at `pos`. Unbuffered on purpose: the walk reads a few 8-byte headers per level
/// (moov, its traks, their mdia/edts) — a few dozen small reads once per
/// track open, and each read SEEKS, which would throw a BufReader's buffer
/// away every time anyway. The large atoms (mdat) are skipped, never read.
fn read_at(f: &mut std::fs::File, pos: u64, n: usize) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(pos)).ok()?;
    let mut buf = vec![0; n];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    #[test]
    fn itunsmpb_gives_delay_and_length() {
        let v = " 00000000 00000840 0000037C 000000000000AC44 00000000 00000000 00000000 00000000 00000000 00000000 00000000 00000000";
        assert_eq!(from_itunsmpb(v), Some(Gapless { delay: 2112, length: Some(44100), padding: 892 }));
        assert_eq!(from_itunsmpb("garbage"), None);
        // A length field of 0: the length comes from the container, minus
        // the priming and the padding.
        let no_len = from_itunsmpb(" 00000000 00000840 0000037C 0000000000000000").unwrap();
        assert_eq!(no_len.length, None);
        assert_eq!(no_len.with_length_from(2112 + 44100 + 892).length, Some(44100));
        assert_eq!(no_len.with_length_from(0).length, None, "no container length: still unknown");
        assert_eq!(from_itunsmpb(" 00000000 00000000 00000000 0000000000000000"), None, "says nothing");
    }

    #[test]
    fn edit_list_gives_delay_and_length() {
        // ffmpeg's AAC: 1024 priming frames, 1 s at 44.1 kHz.
        assert_eq!(
            from_mp4_edit_list(&fixture("sine_aac_editlist.m4a"), 44_100, None),
            Some(Gapless { delay: 1024, length: Some(44100), padding: 0 })
        );
        // The list counts in the file's own timescale (44.1 kHz here), so a
        // decoder running at another rate gets it converted.
        assert_eq!(
            from_mp4_edit_list(&fixture("sine_aac_editlist.m4a"), 88_200, None),
            Some(Gapless { delay: 2048, length: Some(88200), padding: 0 })
        );
        assert_eq!(from_mp4_edit_list(&fixture("sine_lr.flac"), 44_100, None), None, "not an MP4");
        // By track: the fixture's one track is ID 1; any other ID matches nothing.
        let one = from_mp4_edit_list(&fixture("sine_aac_editlist.m4a"), 44_100, Some(1));
        assert_eq!(one.map(|g| g.delay), Some(1024));
        assert_eq!(from_mp4_edit_list(&fixture("sine_aac_editlist.m4a"), 44_100, Some(2)), None);
    }
}
