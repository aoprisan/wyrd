use thiserror::Error;

/// Errors produced while writing or reading a wyrd recording.
#[derive(Debug, Error)]
pub enum WeaveError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("(de)serialization error: {0}")]
    Postcard(#[from] postcard::Error),

    #[error("not a wyrd recording: bad magic bytes")]
    BadMagic,

    #[error("unsupported recording format version: {0} (this build understands {expected})", expected = crate::format::VERSION)]
    UnsupportedVersion(u16),

    #[error("a single event frame exceeded the 4 GiB frame limit")]
    FrameTooLarge,

    #[error("no output file configured on the WeaveLayer builder")]
    NoPath,
}
