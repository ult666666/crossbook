//! Crash-recovery property tests.
//!
//! A session runs uninterrupted while its journal is written and a snapshot is taken
//! every few commands. Then the journal is cut at a random byte, as if the process died
//! in the middle of an append, and recovery starts from the newest snapshot the surviving
//! journal covers. The recovered engine must hold exactly the state the uninterrupted run
//! held after the same number of commands (its snapshot must be byte-identical, which
//! covers the book, the time priorities, the trading phase and the auction prices), and
//! the replayed tail must emit the same events. The sessions include call phases, and one
//! property crashes inside a call phase on purpose. Snapshots must also restore time
//! priority exactly, damaged snapshots must be rejected rather than misread, and files
//! written by crossbook 0.2 must still recover.

mod common;

use common::{call_command, config, session_command, start_call};
use crossbook::recovery::recover;
use crossbook::{Command, Engine, Event, Phase, codec, digest, journal, snapshot};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;

/// Runs `commands` with a snapshot every `every` commands, cuts the journal at `cut` within
/// the bytes after the first `protected` commands, recovers and compares with the
/// uninterrupted run. Returns the recovered engine.
fn crash_and_recover(
    commands: &[Command],
    every: usize,
    protected: usize,
    cut: Index,
) -> Result<Engine, TestCaseError> {
    // The uninterrupted run: each command's events, the full state after every prefix
    // (`states[k]` is the snapshot after k commands) and a snapshot every few commands.
    let mut engine = Engine::new();
    let mut events_per_command: Vec<Vec<Event>> = Vec::new();
    let mut states = vec![snapshot::encode(&engine, 0)];
    let mut snapshots = vec![0usize];
    for (i, command) in commands.iter().enumerate() {
        let mut events = Vec::new();
        engine.process(command, &mut events);
        events_per_command.push(events);
        states.push(snapshot::encode(&engine, (i + 1) as u64));
        if (i + 1) % every == 0 {
            snapshots.push(i + 1);
        }
    }

    // The journal, and the byte offset where each frame ends.
    let mut bytes = Vec::new();
    journal::write(&mut bytes, commands).unwrap();
    let mut ends = vec![journal::MAGIC.len()];
    for command in commands {
        ends.push(ends[ends.len() - 1] + codec::encoded_len(command));
    }

    // The crash keeps a random prefix of the journal past the protected commands.
    let keep = ends[protected] + cut.index(bytes.len() - ends[protected] + 1);
    bytes.truncate(keep);
    let complete = ends.iter().rposition(|&end| end <= keep).unwrap();
    // Write-ahead: a snapshot never includes a command the journal lost.
    let taken = *snapshots.iter().rev().find(|&&k| k <= complete).unwrap();

    let mut replayed = Vec::new();
    let recovery = recover(Some(&states[taken]), &bytes[..], &mut replayed)
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(recovery.sequence, complete as u64);
    prop_assert_eq!(recovery.snapshot_sequence, taken as u64);
    prop_assert_eq!(recovery.journal_len, ends[complete] as u64);
    prop_assert_eq!(recovery.torn_bytes, (keep - ends[complete]) as u64);
    recovery
        .engine
        .check_invariants()
        .map_err(TestCaseError::fail)?;
    prop_assert!(
        snapshot::encode(&recovery.engine, complete as u64) == states[complete],
        "the recovered state differs from the uninterrupted run after {} commands",
        complete
    );
    prop_assert_eq!(replayed, events_per_command[taken..complete].concat());
    Ok(recovery.engine)
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn recovery_after_a_crash_matches_the_uninterrupted_run(
        commands in prop::collection::vec(session_command(), 1..200),
        every in 1usize..40,
        cut in any::<Index>(),
    ) {
        crash_and_recover(&commands, every, 0, cut)?;
    }

    #[test]
    fn a_crash_during_a_call_phase_recovers_into_the_same_call(
        before in prop::collection::vec(session_command(), 0..80),
        open in start_call(),
        during in prop::collection::vec(call_command(), 1..120),
        every in 1usize..30,
        cut in any::<Index>(),
    ) {
        // The journal is cut after the `StartCall`, while the call is still going on.
        let protected = before.len() + 1;
        let commands: Vec<Command> = before.into_iter().chain([open]).chain(during).collect();
        let recovered = crash_and_recover(&commands, every, protected, cut)?;
        prop_assert_eq!(recovered.phase(), Phase::Call);
    }

    #[test]
    fn a_restored_engine_behaves_like_the_original(
        before in prop::collection::vec(session_command(), 0..150),
        after in prop::collection::vec(session_command(), 1..150),
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
        commands in prop::collection::vec(session_command(), 0..80),
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

/// A journal and a snapshot written by crossbook 0.2 (format version 1, before auctions)
/// recover to the digests that version printed: `crossbook gen --orders 400 --seed 5
/// --book 60`, `crossbook snapshot --at 200`, then `replay` and `recover`.
#[test]
fn files_from_crossbook_0_2_still_recover() {
    let journal = include_bytes!("fixtures/v1-journal.bin");
    let image = include_bytes!("fixtures/v1-snapshot.bin");
    assert_eq!(journal::Reader::new(&journal[..]).unwrap().version(), 1);

    let mut events = Vec::new();
    let full = recover(None, &journal[..], &mut events).unwrap();
    assert_eq!(full.sequence, 400);
    assert_eq!(
        format!("{:016x}", digest::events(&events)),
        "d5763cfa77eff0e5"
    );
    assert_eq!(
        format!("{:016x}", full.engine.state_hash()),
        "3034e413d9a12945"
    );

    let mut tail = Vec::new();
    let from_snapshot = recover(Some(&image[..]), &journal[..], &mut tail).unwrap();
    assert_eq!(from_snapshot.snapshot_sequence, 200);
    assert_eq!(
        format!("{:016x}", digest::events(&tail)),
        "74388a9b8e4319b6"
    );
    assert_eq!(from_snapshot.engine.snapshot(), full.engine.snapshot());
    from_snapshot.engine.check_invariants().unwrap();
}
