//! Crash-recovery property tests.
//!
//! A session runs uninterrupted while its journal is written and a snapshot is taken
//! every few commands. Then the journal is cut at a random byte, as if the process died
//! in the middle of an append, and recovery starts from the newest snapshot the surviving
//! journal covers. The recovered engine must hold exactly the book the uninterrupted run
//! held after the same number of commands, and the replayed tail must emit the same
//! events. Snapshots must also restore time priority exactly, and damaged snapshots must
//! be rejected rather than misread.

mod common;

use common::{command, config};
use crossbook::recovery::recover;
use crossbook::{Engine, Event, codec, journal, snapshot};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;

proptest! {
    #![proptest_config(config())]

    #[test]
    fn recovery_after_a_crash_matches_the_uninterrupted_run(
        commands in prop::collection::vec(command(), 1..200),
        every in 1usize..40,
        cut in any::<Index>(),
    ) {
        // The uninterrupted run: each command's events, the book after every prefix
        // (`hashes[k]` is the state after k commands) and a snapshot every few commands.
        let mut engine = Engine::new();
        let mut events_per_command: Vec<Vec<Event>> = Vec::new();
        let mut hashes = vec![engine.state_hash()];
        let mut snapshots = vec![(0usize, snapshot::encode(&engine, 0))];
        for (i, command) in commands.iter().enumerate() {
            let mut events = Vec::new();
            engine.process(command, &mut events);
            events_per_command.push(events);
            hashes.push(engine.state_hash());
            if (i + 1) % every == 0 {
                snapshots.push((i + 1, snapshot::encode(&engine, (i + 1) as u64)));
            }
        }

        // The journal, and the byte offset where each frame ends.
        let mut bytes = Vec::new();
        journal::write(&mut bytes, &commands).unwrap();
        let mut ends = vec![journal::MAGIC.len()];
        for command in &commands {
            ends.push(ends[ends.len() - 1] + codec::encoded_len(command));
        }

        // The crash keeps a random prefix of the journal (the header was written first).
        let keep = journal::MAGIC.len() + cut.index(bytes.len() - journal::MAGIC.len() + 1);
        bytes.truncate(keep);
        let complete = ends.iter().rposition(|&end| end <= keep).unwrap();
        // Write-ahead: a snapshot never includes a command the journal lost.
        let (taken, image) = snapshots.iter().rev().find(|(k, _)| *k <= complete).unwrap();

        let mut replayed = Vec::new();
        let recovery = recover(Some(image), &bytes[..], &mut replayed)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert_eq!(recovery.sequence, complete as u64);
        prop_assert_eq!(recovery.snapshot_sequence, *taken as u64);
        prop_assert_eq!(recovery.journal_len, ends[complete] as u64);
        prop_assert_eq!(recovery.torn_bytes, (keep - ends[complete]) as u64);
        prop_assert_eq!(recovery.engine.state_hash(), hashes[complete]);
        recovery.engine.check_invariants().map_err(TestCaseError::fail)?;
        prop_assert_eq!(replayed, events_per_command[*taken..complete].concat());
    }

    #[test]
    fn a_restored_engine_behaves_like_the_original(
        before in prop::collection::vec(command(), 0..150),
        after in prop::collection::vec(command(), 1..150),
    ) {
        let mut original = Engine::new();
        let mut sink = Vec::new();
        for command in &before {
            original.process(command, &mut sink);
        }
        let (mut restored, _) = snapshot::decode(&snapshot::encode(&original, 0))
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        restored.check_invariants().map_err(TestCaseError::fail)?;
        for command in &after {
            let (mut expected, mut actual) = (Vec::new(), Vec::new());
            original.process(command, &mut expected);
            restored.process(command, &mut actual);
            prop_assert_eq!(expected, actual, "after {:?}", command);
        }
        // Time priorities and the clock survive too, so later snapshots are identical.
        prop_assert_eq!(snapshot::encode(&original, 7), snapshot::encode(&restored, 7));
    }

    #[test]
    fn damaged_snapshots_are_rejected_not_misread(
        commands in prop::collection::vec(command(), 0..80),
        position in any::<Index>(),
        flip in 1u8..=255,
    ) {
        let mut engine = Engine::new();
        let mut sink = Vec::new();
        for command in &commands {
            engine.process(command, &mut sink);
        }
        let mut bytes = snapshot::encode(&engine, 3);
        let at = position.index(bytes.len());
        bytes[at] ^= flip;
        prop_assert!(snapshot::decode(&bytes).is_err());
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..400)) {
        let _ = snapshot::decode(&bytes);
        let mut prefixed = snapshot::MAGIC.to_vec();
        prefixed.extend_from_slice(&bytes);
        let _ = snapshot::decode(&prefixed);
    }
}
