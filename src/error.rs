use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("gain_steps must be non-zero")]
    ZeroGainSteps,

    #[error("source and destination paths must be different")]
    SameSourceDestination,

    #[error("not an MP4/M4A file")]
    NotMp4,

    #[error("no moov box found")]
    NoMoovBox,

    #[error("no AAC audio track found")]
    NoAacTrack,

    #[error("AAC parse error: {message}")]
    AacParse { message: String },

    #[error("AAC parse failed: {warnings} warning(s), no gain locations found")]
    AacParseFailure { warnings: u32 },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
