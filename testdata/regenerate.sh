#!/usr/bin/env bash
# Regenerate testdata/tagged_tone.m4a — a short AAC tone with rich iTunes
# metadata, used by tests/test_python_bindings.py to prove that gain
# adjustment preserves all container metadata byte-for-byte.
set -euo pipefail

cd "$(dirname "$0")"

ffmpeg -hide_banner -loglevel error \
    -f lavfi -i "sine=frequency=440:duration=2:sample_rate=44100" \
    -c:a aac -b:a 128k \
    -metadata title="Gain Test Tone" \
    -metadata artist="m4againpy" \
    -metadata album="Fixtures" \
    -metadata date="2026" \
    -metadata genre="Electronic" \
    -metadata track="3/10" \
    -metadata comment="Used to verify metadata survives gain adjustment." \
    -y tagged_tone.m4a

echo "generated: $(pwd)/tagged_tone.m4a"
