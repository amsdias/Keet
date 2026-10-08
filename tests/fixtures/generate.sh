#!/bin/sh
# Regenerates the committed audio fixtures for the DSP-chain integration tests
# (src/decode.rs, mod chain_tests). Requires ffmpeg with libmp3lame.
#
# Content: 1 second, L = 440 Hz sine, R = 1000 Hz sine, amplitude 0.5
# (ffmpeg's lavfi sine source generates at amplitude 1/8, hence volume=4.0;
# verified peak −6.02 dBFS with astats). The tests assert on these exact
# properties — if you change them, update chain_tests to match.
set -e
cd "$(dirname "$0")"

GRAPH="[0:a][1:a]join=inputs=2:channel_layout=stereo[j];[j]volume=4.0[a]"
SRC_L="sine=frequency=440:duration=1:sample_rate=44100"
SRC_R="sine=frequency=1000:duration=1:sample_rate=44100"

ffmpeg -y -v error -f lavfi -i "$SRC_L" -f lavfi -i "$SRC_R" \
    -filter_complex "$GRAPH" -map "[a]" -sample_fmt s16 sine_lr.flac

ffmpeg -y -v error -f lavfi -i "$SRC_L" -f lavfi -i "$SRC_R" \
    -filter_complex "$GRAPH" -map "[a]" -c:a libmp3lame -b:a 192k sine_lr.mp3

ffmpeg -y -v error -i sine_lr.flac -c copy \
    -metadata REPLAYGAIN_TRACK_GAIN="-6.02 dB" \
    -metadata REPLAYGAIN_TRACK_PEAK="0.500000" sine_lr_rg.flac

# MP3 tagged both ways, as many taggers leave files: ID3v2 at the front (full
# title, ReplayGain in TXXX frames) AND a trailing ID3v1 (title cut to 30
# characters, no ReplayGain). Symphonia reads the trailing block first; Keet
# must still take the ID3v2 values.
ffmpeg -y -v error -i sine_lr.flac -c:a libmp3lame -b:a 128k -id3v2_version 3 -write_id3v1 1 \
    -metadata title="A Title Much Longer Than Thirty Characters" -metadata artist="Fixture Artist" \
    -metadata REPLAYGAIN_TRACK_GAIN="-6.02 dB" -metadata REPLAYGAIN_TRACK_PEAK="0.500000" \
    sine_lr_id3v1v2.mp3

# AAC in MP4, 1 s of 440 Hz stereo at 0.5. An AAC decoder emits priming
# samples first and padding last; the container says how many, in one of two
# ways, and both are tested: ffmpeg writes an edit list (elst), Apple's
# encoder an iTunSMPB tag. afconvert exists only on macOS — keep the
# committed file when regenerating elsewhere.
ffmpeg -y -v error -f lavfi -i "sine=frequency=440:duration=1:sample_rate=44100" \
    -af volume=4.0 -ac 2 -c:a pcm_s16le aac_src.wav
ffmpeg -y -v error -i aac_src.wav -c:a aac -b:a 96k sine_aac_editlist.m4a
if command -v afconvert >/dev/null; then
    afconvert -f m4af -d aac -b 96000 aac_src.wav sine_aac_itunsmpb.m4a
fi
rm aac_src.wav

# Chained Ogg: two complete Ogg Vorbis streams back to back in one file (how
# internet radio rips and concatenated .ogg files look). 1 s of 440 Hz, then
# 1 s of 1000 Hz, both mono-to-stereo at amplitude 0.5. Uses ffmpeg's native
# vorbis encoder (hence -strict -2) so libvorbis is not required.
ffmpeg -y -v error -f lavfi -i "sine=frequency=440:duration=1:sample_rate=44100" \
    -af volume=4.0 -ac 2 -c:a vorbis -strict -2 chain_part1.ogg
ffmpeg -y -v error -f lavfi -i "sine=frequency=1000:duration=1:sample_rate=44100" \
    -af volume=4.0 -ac 2 -c:a vorbis -strict -2 chain_part2.ogg
cat chain_part1.ogg chain_part2.ogg > chained.ogg
rm chain_part1.ogg chain_part2.ogg

# Hi-res FLAC for the bit-perfect test: 24-bit / 96 kHz, 0.25 s, L = 997 Hz,
# R = 3001 Hz (co-prime with the rate, so every code path sees varied samples),
# near full scale (volume 7.9 x lavfi's 1/8 = 0.9875).
ffmpeg -y -v error -f lavfi -i "sine=frequency=997:duration=0.25:sample_rate=96000" \
    -f lavfi -i "sine=frequency=3001:duration=0.25:sample_rate=96000" \
    -filter_complex "[0:a][1:a]join=inputs=2:channel_layout=stereo[j];[j]volume=7.9[a]" \
    -map "[a]" -sample_fmt s32 -bits_per_raw_sample 24 -c:a flac hires_24_96.flac

echo "fixtures regenerated:"
ls -la sine_lr.flac sine_lr.mp3 sine_lr_rg.flac sine_lr_id3v1v2.mp3 sine_aac_editlist.m4a sine_aac_itunsmpb.m4a chained.ogg hires_24_96.flac
