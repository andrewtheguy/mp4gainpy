//! Bare-minimum AAC/M4A `global_gain` rewriter.
//!
//! Two public functions:
//!   - [`aac_apply_gain`] – in-memory bytes → bytes.
//!   - [`aac_apply_gain_file`] – streamed file → file rewrite.

pub const GAIN_STEP_DB: f64 = 1.5;

mod aac;
mod aac_codebooks;
mod bits;
mod error;
mod gain;
mod mp4;

pub use error::{Error, Result};
pub use gain::{aac_apply_gain, aac_apply_gain_file};

#[cfg(feature = "python")]
mod python;
