//! On-disk recording format: a small header followed by length-prefixed
//! postcard frames.
//!
//! ```text
//! ┌────────┬─────────┬──────────────────────────────────────────┐
//! │ "WYRD" │ ver: u16│ frame*  ( len: u32 LE ‖ postcard(Record) )│
//! └────────┴─────────┴──────────────────────────────────────────┘
//! ```

use std::fs::File;
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::Path;

use crate::error::WeaveError;
use crate::event::Record;

/// Magic bytes at the start of every recording.
pub const MAGIC: &[u8; 4] = b"WYRD";

/// Current on-disk format version.
pub const VERSION: u16 = 1;

/// Writes a recording header then length-prefixed frames.
pub struct FrameWriter<W: Write> {
    inner: W,
}

impl<W: Write> FrameWriter<W> {
    /// Create a writer, emitting the file header immediately.
    pub fn new(mut inner: W) -> Result<Self, WeaveError> {
        inner.write_all(MAGIC)?;
        inner.write_all(&VERSION.to_le_bytes())?;
        Ok(Self { inner })
    }

    /// Append one length-prefixed frame.
    pub fn write_record(&mut self, record: &Record) -> Result<(), WeaveError> {
        let bytes = postcard::to_stdvec(record)?;
        let len = u32::try_from(bytes.len()).map_err(|_| WeaveError::FrameTooLarge)?;
        self.inner.write_all(&len.to_le_bytes())?;
        self.inner.write_all(&bytes)?;
        Ok(())
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> Result<(), WeaveError> {
        self.inner.flush()?;
        Ok(())
    }
}

/// Reads records written by a [`FrameWriter`].
pub struct FrameReader<R: Read> {
    inner: R,
}

impl<R: Read> FrameReader<R> {
    /// Create a reader, validating the header.
    pub fn new(mut inner: R) -> Result<Self, WeaveError> {
        let mut magic = [0u8; 4];
        inner.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(WeaveError::BadMagic);
        }
        let mut ver = [0u8; 2];
        inner.read_exact(&mut ver)?;
        let ver = u16::from_le_bytes(ver);
        if ver != VERSION {
            return Err(WeaveError::UnsupportedVersion(ver));
        }
        Ok(Self { inner })
    }

    /// Read the next record, or `None` at clean end-of-file.
    pub fn next_record(&mut self) -> Result<Option<Record>, WeaveError> {
        let mut len_buf = [0u8; 4];
        match self.inner.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        self.inner.read_exact(&mut buf)?;
        let record = postcard::from_bytes(&buf)?;
        Ok(Some(record))
    }
}

impl<R: Read> Iterator for FrameReader<R> {
    type Item = Result<Record, WeaveError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_record().transpose()
    }
}

/// Read an entire recording file into memory.
pub fn read_records(path: impl AsRef<Path>) -> Result<Vec<Record>, WeaveError> {
    let file = File::open(path)?;
    let mut reader = FrameReader::new(BufReader::new(file))?;
    let mut out = Vec::new();
    while let Some(record) = reader.next_record()? {
        out.push(record);
    }
    Ok(out)
}

/// Convenience: open a file for writing and wrap it in a buffered [`FrameWriter`].
pub fn file_writer(path: impl AsRef<Path>) -> Result<FrameWriter<BufWriter<File>>, WeaveError> {
    let file = File::create(path)?;
    FrameWriter::new(BufWriter::new(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Loc, TaskKind};

    #[test]
    fn header_and_frame_roundtrip() {
        let records = vec![
            Record {
                ts: 1,
                event: Event::TaskSpawn {
                    id: 7,
                    parent: Some(6),
                    name: Some("parent".into()),
                    loc: Loc {
                        file: Some("src/main.rs".into()),
                        line: Some(161),
                        col: Some(10),
                    },
                    kind: TaskKind::Task,
                },
            },
            Record {
                ts: 2,
                event: Event::PollStart { task: 7 },
            },
        ];

        let mut buf = Vec::new();
        {
            let mut w = FrameWriter::new(&mut buf).unwrap();
            for r in &records {
                w.write_record(r).unwrap();
            }
            w.flush().unwrap();
        }

        // Header is present and correct.
        assert_eq!(&buf[0..4], MAGIC);
        assert_eq!(u16::from_le_bytes([buf[4], buf[5]]), VERSION);

        let mut reader = FrameReader::new(&buf[..]).unwrap();
        let mut got = Vec::new();
        while let Some(r) = reader.next_record().unwrap() {
            got.push(r);
        }
        assert_eq!(records, got);
    }

    #[test]
    fn rejects_bad_magic() {
        let buf = b"NOPExxxxxx";
        match FrameReader::new(&buf[..]) {
            Err(WeaveError::BadMagic) => {}
            other => panic!("expected BadMagic, got {:?}", other.map(|_| ())),
        }
    }
}
