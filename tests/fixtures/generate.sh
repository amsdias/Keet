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

echo "fixtures regenerated:"
ls -la sine_lr.flac sine_lr.mp3 sine_lr_rg.flac chained.ogg
