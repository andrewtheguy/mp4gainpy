use std::fs::{self, File, OpenOptions};
use std::path::Path;

use crate::aac;
use crate::error::{Error, Result};
use crate::mp4;

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

/// Stream AAC/M4A from `src`, apply a static `gain_steps`, and write the
/// modified bytes to `dst`. Returns the number of `global_gain` locations
/// actually modified.
///
/// `src` is never modified, and `dst` must refer to a different path/file.
/// Errors if `gain_steps == 0`.
pub fn aac_apply_gain_file(src: &Path, dst: &Path, gain_steps: i32) -> Result<usize> {
    if gain_steps == 0 {
        return Err(Error::ZeroGainSteps);
    }

    ensure_distinct_paths(src, dst)?;

    let mut src_file = File::open(src)?;
    let gain_plan = aac::analyze_file(&mut src_file)?;

    let mut dst_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;

    let modified =
        aac::apply_gain_plan_to_file(&mut src_file, &mut dst_file, &gain_plan, gain_steps)?;
    mp4::write_gain_metadata(&mut dst_file, gain_steps)?;
    Ok(modified)
}

fn ensure_distinct_paths(src: &Path, dst: &Path) -> Result<()> {
    if src == dst {
        return Err(Error::SameSourceDestination);
    }

    let src_canonical = fs::canonicalize(src)?;

    if let Ok(dst_canonical) = fs::canonicalize(dst) {
        if src_canonical == dst_canonical {
            return Err(Error::SameSourceDestination);
        }
    }

    if let Ok(dst_metadata) = fs::metadata(dst) {
        let src_metadata = fs::metadata(&src_canonical)?;
        if same_file_metadata(&src_metadata, &dst_metadata) {
            return Err(Error::SameSourceDestination);
        }
    }

    Ok(())
}

#[cfg(unix)]
fn same_file_metadata(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file_metadata(_a: &fs::Metadata, _b: &fs::Metadata) -> bool {
    false
}
