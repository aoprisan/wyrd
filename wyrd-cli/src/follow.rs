//! Tolerant loading of a recording that may still be growing.
//!
//! Shared by `wyrd tui --follow` and `wyrd watch`: ingest every *complete*
//! frame and stop cleanly at a torn tail frame or an as-yet-unflushed header.
//! The producer is never touched — all cost is in the observer process.

use std::io::Cursor;
use std::path::Path;

use wyrd_core::Recording;
use wyrd_weave::FrameReader;

/// Read a recording that may be mid-write. Missing or unreadable files fold
/// to an empty recording (the caller keeps waiting for data).
pub(crate) fn load_follow(path: &Path) -> Recording {
    match std::fs::read(path) {
        Ok(bytes) => recording_from_bytes(&bytes),
        Err(_) => empty_recording(),
    }
}

pub(crate) fn recording_from_bytes(bytes: &[u8]) -> Recording {
    let Ok(mut reader) = FrameReader::new(Cursor::new(bytes)) else {
        // Header not written (or only partially) yet.
        return empty_recording();
    };
    let mut records = Vec::new();
    // `next_record` yields `Ok(None)` at a clean boundary and `Err(_)` on a
    // half-written tail frame; either way we keep the complete prefix.
    while let Ok(Some(record)) = reader.next_record() {
        records.push(record);
    }
    Recording::from_records(records).unwrap_or_else(|_| empty_recording())
}

pub(crate) fn empty_recording() -> Recording {
    Recording::from_records(std::iter::empty()).expect("empty recording is always valid")
}
