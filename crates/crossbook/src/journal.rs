//! Command journals: an 8-byte header followed by [`codec`] frames.
//!
//! The engine's state is a pure function of its input, so a journal of commands is a
//! complete record of a session. Replaying it reproduces every event and the final book;
//! the CLI's `replay` command prints both fingerprints so two runs can be compared.

use core::fmt;
use std::io::{self, BufRead, Write};

use crate::codec::{self, DecodeError, NEW_LEN};
use crate::types::Command;

/// File signature and format version.
pub const MAGIC: [u8; 8] = *b"XBOOK\x00\x00\x01";

/// Why a journal could not be read.
#[derive(Debug)]
pub enum JournalError {
    /// The underlying reader failed.
    Io(io::Error),
    /// The file does not start with [`MAGIC`].
    BadMagic,
    /// A frame at `offset` failed to decode.
    Decode {
        /// Byte offset of the frame within the file.
        offset: u64,
        /// Decoder error.
        error: DecodeError,
    },
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "journal i/o error: {error}"),
            Self::BadMagic => f.write_str("not a crossbook journal (bad header)"),
            Self::Decode { offset, error } => write!(f, "bad frame at byte {offset}: {error}"),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Decode { error, .. } => Some(error),
            Self::BadMagic => None,
        }
    }
}

impl From<io::Error> for JournalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Writes a journal header followed by `commands`.
///
/// # Errors
/// Propagates write errors.
pub fn write<'a, W: Write>(
    mut writer: W,
    commands: impl IntoIterator<Item = &'a Command>,
) -> io::Result<()> {
    writer.write_all(&MAGIC)?;
    let mut frame = Vec::with_capacity(NEW_LEN);
    for command in commands {
        frame.clear();
        codec::encode(command, &mut frame);
        writer.write_all(&frame)?;
    }
    writer.flush()
}

/// Streaming journal reader. Yields one command per frame.
#[derive(Debug)]
pub struct Reader<R> {
    inner: R,
    offset: u64,
    failed: bool,
}

impl<R: BufRead> Reader<R> {
    /// Checks the header and returns a reader positioned at the first frame.
    ///
    /// # Errors
    /// Returns [`JournalError::BadMagic`] for a missing or wrong header.
    pub fn new(mut inner: R) -> Result<Self, JournalError> {
        let mut magic = [0u8; MAGIC.len()];
        match inner.read_exact(&mut magic) {
            Ok(()) if magic == MAGIC => {}
            Ok(()) => return Err(JournalError::BadMagic),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(JournalError::BadMagic);
            }
            Err(error) => return Err(error.into()),
        }
        Ok(Self {
            inner,
            offset: MAGIC.len() as u64,
            failed: false,
        })
    }

    fn next_frame(&mut self) -> Result<Option<Command>, JournalError> {
        let mut frame = [0u8; NEW_LEN];
        if self.inner.read(&mut frame[..1])? == 0 {
            return Ok(None);
        }
        let len = match frame[0] {
            1 => codec::NEW_LEN,
            2 => codec::CANCEL_LEN,
            3 => codec::MODIFY_LEN,
            other => {
                return Err(JournalError::Decode {
                    offset: self.offset,
                    error: DecodeError::UnknownType(other),
                });
            }
        };
        let mut filled = 1;
        while filled < len {
            match self.inner.read(&mut frame[filled..len])? {
                0 => {
                    return Err(JournalError::Decode {
                        offset: self.offset,
                        error: DecodeError::Truncated {
                            needed: len,
                            available: filled,
                        },
                    });
                }
                read => filled += read,
            }
        }
        let (command, _) = codec::decode(&frame[..len]).map_err(|error| JournalError::Decode {
            offset: self.offset,
            error,
        })?;
        self.offset += len as u64;
        Ok(Some(command))
    }
}

impl<R: BufRead> Iterator for Reader<R> {
    type Item = Result<Command, JournalError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        match self.next_frame() {
            Ok(command) => command.map(Ok),
            Err(error) => {
                self.failed = true;
                Some(Err(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{NewOrder, Side};

    #[test]
    fn round_trips_and_reports_offsets() {
        let commands = [
            Command::New(NewOrder::limit(1, Side::Buy, 100, 5)),
            Command::Modify {
                id: 1,
                price: 101,
                qty: 4,
            },
            Command::Cancel { id: 1 },
        ];
        let mut bytes = Vec::new();
        write(&mut bytes, &commands).unwrap();
        let read: Vec<_> = Reader::new(&bytes[..])
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(read, commands);

        bytes.truncate(bytes.len() - 3);
        let mut reader = Reader::new(&bytes[..]).unwrap();
        assert!(reader.next().unwrap().is_ok());
        assert!(reader.next().unwrap().is_ok());
        match reader.next() {
            Some(Err(JournalError::Decode { offset, .. })) => assert_eq!(offset, 8 + 40 + 32),
            other => panic!("expected a decode error, got {other:?}"),
        }
        assert!(reader.next().is_none());
        assert!(matches!(
            Reader::new(&b"XBOOK"[..]),
            Err(JournalError::BadMagic)
        ));
    }
}
