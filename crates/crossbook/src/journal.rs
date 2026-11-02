//! Command journals: an 8-byte header followed by [`codec`] frames.
//!
//! The engine's state is a pure function of its input, so a journal of commands is a
//! complete record of a session. Replaying it reproduces every event and the final book;
//! the CLI's `replay` command prints both fingerprints so two runs can be compared.
//!
//! The header's last byte is the format version. Version 1 holds `New`, `Cancel` and
//! `Modify` frames; version 2 adds the phase commands of call auctions (`StartCall`,
//! `Uncross` and `Resume`). [`write()`] produces version 2 and [`Reader`] reads both, so
//! journals written before auctions existed replay unchanged.

use core::fmt;
use std::io::{self, BufRead, Write};

use crate::codec::{self, DecodeError, MAX_LEN};
use crate::types::Command;

/// File signature and the format version this build writes.
pub const MAGIC: [u8; 8] = *b"XBOOK\x00\x00\x02";

/// The format version this build writes.
pub const VERSION: u8 = MAGIC[MAGIC.len() - 1];

/// The last frame type of version 1, before call auctions.
const V1_LAST_TYPE: u8 = 3;

/// Why a journal could not be read.
#[derive(Debug)]
pub enum JournalError {
    /// The underlying reader failed.
    Io(io::Error),
    /// The file does not start with the journal signature.
    BadMagic,
    /// The file is a journal in a format version this build does not read.
    UnsupportedVersion(u8),
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
            Self::UnsupportedVersion(version) => write!(
                f,
                "journal format version {version} is not supported (this build reads 1 to {VERSION})"
            ),
            Self::Decode { offset, error } => write!(f, "bad frame at byte {offset}: {error}"),
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Decode { error, .. } => Some(error),
            Self::BadMagic | Self::UnsupportedVersion(_) => None,
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
    let mut frame = Vec::with_capacity(MAX_LEN);
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
    version: u8,
    failed: bool,
}

impl<R: BufRead> Reader<R> {
    /// Checks the header and returns a reader positioned at the first frame.
    ///
    /// # Errors
    /// Returns [`JournalError::BadMagic`] for a missing or wrong header and
    /// [`JournalError::UnsupportedVersion`] for a version newer than this build.
    pub fn new(mut inner: R) -> Result<Self, JournalError> {
        let mut magic = [0u8; MAGIC.len()];
        match inner.read_exact(&mut magic) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(JournalError::BadMagic);
            }
            Err(error) => return Err(error.into()),
        }
        let (signature, version) = (&magic[..MAGIC.len() - 1], magic[MAGIC.len() - 1]);
        if signature != &MAGIC[..MAGIC.len() - 1] {
            return Err(JournalError::BadMagic);
        }
        if !(1..=VERSION).contains(&version) {
            return Err(JournalError::UnsupportedVersion(version));
        }
        Ok(Self {
            inner,
            offset: MAGIC.len() as u64,
            version,
            failed: false,
        })
    }

    /// Bytes read so far, header included: the end of the last complete frame.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// The journal's format version.
    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }

    fn next_frame(&mut self) -> Result<Option<Command>, JournalError> {
        let mut frame = [0u8; MAX_LEN];
        if self.inner.read(&mut frame[..1])? == 0 {
            return Ok(None);
        }
        // Version 1 journals predate the phase commands, so their type bytes are unknown there.
        let known = self.version > 1 || frame[0] <= V1_LAST_TYPE;
        let Some(len) = codec::frame_len(frame[0]).filter(|_| known) else {
            return Err(JournalError::Decode {
                offset: self.offset,
                error: DecodeError::UnknownType(frame[0]),
            });
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

    #[test]
    fn reads_version_1_and_refuses_unknown_versions() {
        let commands = [
            Command::StartCall {
                reference: Some(100),
            },
            Command::New(NewOrder::limit(1, Side::Buy, 100, 5)),
            Command::Uncross,
            Command::Resume,
        ];
        let mut bytes = Vec::new();
        write(&mut bytes, &commands).unwrap();
        let reader = Reader::new(&bytes[..]).unwrap();
        assert_eq!(reader.version(), VERSION);
        let read: Vec<_> = reader.map(Result::unwrap).collect();
        assert_eq!(read, commands);

        // The same bytes under a version 1 header: the phase commands did not exist yet.
        let mut v1 = bytes.clone();
        v1[7] = 1;
        let mut reader = Reader::new(&v1[..]).unwrap();
        assert_eq!(reader.version(), 1);
        assert!(matches!(
            reader.next(),
            Some(Err(JournalError::Decode {
                offset: 8,
                error: DecodeError::UnknownType(4)
            }))
        ));
        let mut v1 = Vec::new();
        write(&mut v1, &commands[1..2]).unwrap();
        v1[7] = 1;
        let read: Vec<_> = Reader::new(&v1[..]).unwrap().map(Result::unwrap).collect();
        assert_eq!(read, commands[1..2]);

        for version in [0, VERSION + 1] {
            let mut future = bytes.clone();
            future[7] = version;
            let error = Reader::new(&future[..]).unwrap_err();
            assert!(matches!(error, JournalError::UnsupportedVersion(v) if v == version));
            assert!(error.to_string().contains("not supported"));
        }
        let mut wrong = bytes;
        wrong[0] = b'Y';
        assert!(matches!(
            Reader::new(&wrong[..]),
            Err(JournalError::BadMagic)
        ));
    }
}
