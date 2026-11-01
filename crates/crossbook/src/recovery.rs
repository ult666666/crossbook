//! Crash recovery: the latest snapshot plus the journal rebuild the engine.
//!
//! The protocol is write-ahead logging. A command is appended to the journal before the
//! engine applies it, and a snapshot records how many journaled commands its book
//! includes. After a crash, [`recover`] loads the snapshot, skips that many commands in
//! the journal and replays the rest. Because the engine is deterministic, the replay
//! emits the same events the lost process emitted and ends in the same book.
//!
//! A crash in the middle of an append leaves a torn final frame. That command was never
//! applied, so recovery stops before it and reports where the intact journal ends; the
//! caller truncates the file there before appending again. A damaged frame anywhere
//! else, or a snapshot that claims more commands than the journal holds, is corruption
//! and stops recovery with an error instead of guessing.

use core::fmt;
use std::io::BufRead;

use crate::Engine;
use crate::codec::DecodeError;
use crate::journal::{self, JournalError};
use crate::snapshot::{self, SnapshotError};
use crate::types::Event;

/// The engine rebuilt by [`recover`] and where its journal stands.
#[derive(Debug)]
pub struct Recovery {
    /// The engine after the snapshot and every intact journaled command after it.
    pub engine: Engine,
    /// Commands the snapshot already included (0 without a snapshot).
    pub snapshot_sequence: u64,
    /// Commands applied in total: the snapshot's plus the replayed ones.
    pub sequence: u64,
    /// Length of the intact journal in bytes; appends continue from here.
    pub journal_len: u64,
    /// Bytes of a torn final frame that were ignored (0 for a clean journal).
    pub torn_bytes: u64,
}

/// Why recovery stopped.
#[derive(Debug)]
pub enum RecoveryError {
    /// The snapshot is damaged or invalid.
    Snapshot(SnapshotError),
    /// The journal is unreadable or damaged before its last frame.
    Journal(JournalError),
    /// The snapshot includes more commands than the journal holds.
    JournalBehindSnapshot {
        /// Commands the snapshot includes.
        snapshot: u64,
        /// Intact commands in the journal.
        journal: u64,
    },
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot(error) => write!(f, "cannot use snapshot: {error}"),
            Self::Journal(error) => write!(f, "cannot replay journal: {error}"),
            Self::JournalBehindSnapshot { snapshot, journal } => write!(
                f,
                "snapshot includes {snapshot} commands but the journal holds only {journal}"
            ),
        }
    }
}

impl std::error::Error for RecoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Snapshot(error) => Some(error),
            Self::Journal(error) => Some(error),
            Self::JournalBehindSnapshot { .. } => None,
        }
    }
}

impl From<SnapshotError> for RecoveryError {
    fn from(error: SnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

impl From<JournalError> for RecoveryError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

/// Rebuilds the engine from an optional snapshot and the journal that produced it.
///
/// The events of the replayed commands are appended to `out`, in order, exactly as the
/// original process emitted them.
///
/// # Errors
/// Returns [`RecoveryError`] for a bad snapshot, a journal damaged before its final
/// frame, or a journal shorter than the snapshot's sequence.
pub fn recover<R: BufRead>(
    snapshot: Option<&[u8]>,
    journal: R,
    out: &mut Vec<Event>,
) -> Result<Recovery, RecoveryError> {
    let (mut engine, snapshot_sequence) = match snapshot {
        Some(bytes) => snapshot::decode(bytes)?,
        None => (Engine::new(), 0),
    };
    let mut reader = journal::Reader::new(journal)?;
    let mut sequence = 0u64;
    let mut torn_bytes = 0u64;
    loop {
        match reader.next() {
            None => break,
            Some(Ok(command)) => {
                sequence += 1;
                if sequence > snapshot_sequence {
                    engine.process(&command, out);
                }
            }
            // The reader only reports a truncated frame when the input ends inside it.
            Some(Err(JournalError::Decode {
                error: DecodeError::Truncated { available, .. },
                ..
            })) => {
                torn_bytes = available as u64;
                break;
            }
            Some(Err(error)) => return Err(error.into()),
        }
    }
    if sequence < snapshot_sequence {
        return Err(RecoveryError::JournalBehindSnapshot {
            snapshot: snapshot_sequence,
            journal: sequence,
        });
    }
    Ok(Recovery {
        engine,
        snapshot_sequence,
        sequence,
        journal_len: reader.offset(),
        torn_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Command, NewOrder, Side};

    fn session() -> Vec<Command> {
        vec![
            Command::New(NewOrder::limit(1, Side::Sell, 101, 5)),
            Command::New(NewOrder::limit(2, Side::Sell, 102, 5)),
            Command::New(NewOrder::limit(3, Side::Buy, 102, 7)),
            Command::Modify {
                id: 2,
                price: 103,
                qty: 2,
            },
            Command::Cancel { id: 2 },
            Command::New(NewOrder::limit(4, Side::Buy, 100, 1)),
        ]
    }

    #[test]
    fn snapshot_plus_tail_matches_the_uninterrupted_run() {
        let commands = session();
        let mut journal = Vec::new();
        journal::write(&mut journal, &commands).unwrap();
        let (mut full, mut events) = (Engine::new(), Vec::new());
        let mut snapshot = Vec::new();
        for (i, command) in commands.iter().enumerate() {
            if i == 3 {
                snapshot = snapshot::encode(&full, 3);
                events.clear();
            }
            full.process(command, &mut events);
        }
        let mut replayed = Vec::new();
        let recovery = recover(Some(&snapshot), &journal[..], &mut replayed).unwrap();
        assert_eq!(replayed, events, "the tail replays the same events");
        assert_eq!(recovery.engine.snapshot(), full.snapshot());
        assert_eq!(
            (recovery.snapshot_sequence, recovery.sequence),
            (3, commands.len() as u64)
        );
        assert_eq!(
            (recovery.journal_len, recovery.torn_bytes),
            (journal.len() as u64, 0)
        );
    }

    #[test]
    fn a_torn_final_frame_is_ignored_but_damage_is_not() {
        let commands = session();
        let mut journal = Vec::new();
        journal::write(&mut journal, &commands).unwrap();
        let mut torn = journal.clone();
        torn.truncate(journal.len() - 5);
        let recovery = recover(None, &torn[..], &mut Vec::new()).unwrap();
        assert_eq!(recovery.sequence, commands.len() as u64 - 1);
        assert_eq!(
            recovery.journal_len + recovery.torn_bytes,
            torn.len() as u64
        );

        let mut damaged = journal.clone();
        damaged[8] = 9; // the first frame's type byte
        assert!(matches!(
            recover(None, &damaged[..], &mut Vec::new()),
            Err(RecoveryError::Journal(JournalError::Decode {
                offset: 8,
                ..
            }))
        ));

        let ahead = snapshot::encode(&Engine::new(), 99);
        assert!(matches!(
            recover(Some(&ahead), &journal[..], &mut Vec::new()),
            Err(RecoveryError::JournalBehindSnapshot {
                snapshot: 99,
                journal: 6
            })
        ));
    }
}
