# m4againpy

Minimal Python + Rust library for **fixed-step gain adjustment of AAC audio in
M4A/MP4 files**. It is based on https://github.com/M-Igashi/mp3rgain, narrowed
to AAC gain rewriting only: no MP3 support, loudness analysis, replaygain tags,
or undo metadata.

The library finds AAC `global_gain` fields in the bitstream and adds or
subtracts the requested number of native AAC gain steps. Unlike the original
implementation, the file API streams source audio to a separate destination
file instead of modifying the input in place; it patches only the gain bits and
records the applied step in a custom MP4 metadata tag.

## Installation

Prebuilt wheels (Linux x86_64/arm64, macOS arm64, Windows x86_64) are published
via GitHub Pages as a PEP 503 index:

```bash
pip install m4againpy --extra-index-url https://andrewtheguy.github.io/m4againpy/simple/
```

Or with [uv](https://docs.astral.sh/uv/):

```bash
uv pip install m4againpy --extra-index-url https://andrewtheguy.github.io/m4againpy/simple/
```

Requires Python ≥ 3.9 (abi3 wheels).

### From source

Needs a Rust toolchain (stable) and [maturin](https://www.maturin.rs/):

```bash
git clone https://github.com/andrewtheguy/m4againpy.git
cd m4againpy
uv venv
uv pip install maturin
uv run maturin develop --features python --release
```

## Usage

```python
import m4againpy

# Bytes in, bytes out
with open("track.m4a", "rb") as f:
    data = f.read()
louder = m4againpy.aac_apply_gain(data, 2)   # +2 steps  (~+3.0 dB)
softer = m4againpy.aac_apply_gain(data, -2)  # -2 steps  (~-3.0 dB)

# File: stream src, apply gain, write a different dst. src is never overwritten.
m4againpy.aac_apply_gain_file("track.m4a", "track_louder.m4a", 2)

# gain_steps == 0 raises RuntimeError in both variants.
# Passing the same source and destination path also raises RuntimeError.

# Step size is 1.5 dB by AAC spec
m4againpy.GAIN_STEP_DB  # 1.5
```

## Units

`gain_steps` is the native AAC `global_gain` unit (an 8-bit integer in the
bitstream). One step is 1.5 dB. If you want to think in dB, just divide:
`steps = round(db / m4againpy.GAIN_STEP_DB)`.

Zero steps is a no-op; gain locations are saturating-clamped to `0..=255`;
locations with `global_gain == 0` are skipped (silence).

The file API writes custom MP4 metadata to the destination:
`TAG:M4AG=m4againpy version=1 gain_steps=<n> gain_step_db=1.5`.
Use `ffprobe -export_all 1` to show the custom tag.

## Development

```bash
uv venv
uv run --no-project --with 'maturin>=1.9.4,<2.0' maturin develop --skip-install --features python
uv run --no-sync python -m unittest tests/test_python_bindings.py -v
```

The Python binding tests load the built extension from `target/debug` or
`target/release`; they do not import an installed `site-packages` copy.

The `tests/testdata/tagged_tone.m4a` fixture is committed; to regenerate it
with ffmpeg, run `testdata/regenerate.sh`.
