# mp4gainpy

Minimal Python + Rust library for **static gain adjustment of AAC/M4A audio** —
no analysis, no undo tags, no metadata. Just locate the `global_gain` fields in
the AAC bitstream and add/subtract a fixed number of steps.

## Installation

Prebuilt wheels (Linux x86_64/arm64, macOS arm64, Windows x86_64) are published
via GitHub Pages as a PEP 503 index:

```bash
pip install mp4gainpy --extra-index-url https://andrewtheguy.github.io/mp4gainpy/simple/
```

Or with [uv](https://docs.astral.sh/uv/):

```bash
uv pip install mp4gainpy --extra-index-url https://andrewtheguy.github.io/mp4gainpy/simple/
```

Requires Python ≥ 3.9 (abi3 wheels).

### From source

Needs a Rust toolchain (stable) and [maturin](https://www.maturin.rs/):

```bash
git clone https://github.com/andrewtheguy/mp4gainpy.git
cd mp4gainpy
uv venv
uv pip install maturin
uv run maturin develop --features python --release
```

## Usage

```python
import mp4gainpy

# Bytes in, bytes out
with open("track.m4a", "rb") as f:
    data = f.read()
louder = mp4gainpy.aac_apply_gain(data, 2)   # +2 steps  (~+3.0 dB)
softer = mp4gainpy.aac_apply_gain(data, -2)  # -2 steps  (~-3.0 dB)

# File: stream src, apply gain, write a different dst. src is never overwritten.
mp4gainpy.aac_apply_gain_file("track.m4a", "track_louder.m4a", 2)

# gain_steps == 0 raises RuntimeError in both variants.
# Passing the same source and destination path also raises RuntimeError.

# Step size is 1.5 dB by AAC spec
mp4gainpy.GAIN_STEP_DB  # 1.5
```

## Units

`gain_steps` is the native AAC `global_gain` unit (an 8-bit integer in the
bitstream). One step is 1.5 dB. If you want to think in dB, just divide:
`steps = round(db / mp4gainpy.GAIN_STEP_DB)`.

Zero steps is a no-op; gain locations are saturating-clamped to `0..=255`;
locations with `global_gain == 0` are skipped (silence).

The file API appends a small top-level MP4 `uuid` box
(`95a58770-4ba7-42ee-9e88-340e58bf3580`) to the destination with the applied
`gain_steps` and `gain_step_db`. This records the operation without rewriting
`moov` metadata or moving existing media data.

## Development

```bash
uv venv
uv run --no-project --with 'maturin>=1.9.4,<2.0' maturin develop --skip-install --features python
uv run --no-sync python -m unittest tests/test_python_bindings.py -v
```

The test loader also runs the `maturin develop --skip-install` build itself
and imports the generated extension from `target/maturin`, not from an
installed `site-packages` copy.

The `tests/testdata/tagged_tone.m4a` fixture is committed; to regenerate it
with ffmpeg, run `testdata/regenerate.sh`.
