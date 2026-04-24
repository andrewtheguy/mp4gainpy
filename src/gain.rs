use std::path::Path;

use crate::aac;
use crate::error::{Error, Result};

/// Apply a static `gain_steps` (1 step ≈ 1.5 dB) to AAC/M4A bytes.
///
/// Returns a new `Vec<u8>` with `global_gain` fields rewritten. Saturating
/// at 0..=255; silence (gain == 0) is skipped. Errors if `gain_steps == 0`.
pub fn aac_apply_gain(data: &[u8], gain_steps: i32) -> Result<Vec<u8>> {
    if gain_steps == 0 {
        return Err(Error::ZeroGainSteps);
    }
    let mut out = data.to_vec();
    aac::apply_gain_to_bytes(&mut out, gain_steps)?;
    Ok(out)
}

/// Read AAC/M4A from `src`, apply a static `gain_steps`, and write the
/// modified bytes to `dst`. Returns the number of `global_gain` locations
/// actually modified.
///
/// `src` is never modified. If `src == dst`, the file is overwritten —
/// passing identical paths is the caller's choice. Errors if
/// `gain_steps == 0`.
pub fn aac_apply_gain_file(src: &Path, dst: &Path, gain_steps: i32) -> Result<usize> {
    if gain_steps == 0 {
        return Err(Error::ZeroGainSteps);
    }
    let mut data = std::fs::read(src)?;
    let modified = aac::apply_gain_to_bytes(&mut data, gain_steps)?;
    std::fs::write(dst, &data)?;
    Ok(modified)
}
