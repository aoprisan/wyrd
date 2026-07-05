use thiserror::Error;

/// Errors from ingesting or querying a recording.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("recording error: {0}")]
    Weave(#[from] wyrd_weave::WeaveError),

    #[error("no task matches {0:?}")]
    UnknownTask(String),

    #[error("recording contains no tasks")]
    Empty,
}
