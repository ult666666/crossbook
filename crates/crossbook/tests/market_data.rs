//! Market-data property tests.
//!
//! The feed is derived from events alone, so the strongest check is end to end: after
//! every command, a subscriber that applied the messages must hold exactly the engine's
//! book, level by level, and the trade prints must mirror the trade events. A subscriber
//! that loses one message must notice by the end of that command, from a later message
//! or the heartbeat, and must hold the engine's book again after resynchronizing.

mod common;

use common::{command, config};
use crossbook::flow::{Flow, FlowConfig};
use crossbook::market_data::{BookBuilder, Message, Publisher};
use crossbook::{Command, Engine, Event, Side};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;

/// Trade prints first, in event order; then level updates, bids before asks, best first.
fn check_order(messages: &[Message]) -> Result<(), TestCaseError> {
    let first_level = messages
        .iter()
        .position(|message| matches!(message, Message::Level { .. }))
        .unwrap_or(messages.len());
    prop_assert!(
        messages[first_level..]
            .iter()
            .all(|message| matches!(message, Message::Level { .. })),
        "a trade print after a level update: {:?}",
        messages
    );
    let keys: Vec<(u8, i64)> = messages[first_level..]
        .iter()
        .map(|message| match *message {
            Message::Level {
                side: Side::Buy,
                price,
                ..
            } => (0, -price),
            Message::Level { price, .. } => (1, price),
            Message::Trade { .. } => unreachable!(),
        })
        .collect();
    prop_assert!(
        keys.windows(2).all(|pair| pair[0] < pair[1]),
        "level updates out of order: {:?}",
        messages
    );
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn subscriber_book_equals_the_engine_after_every_command(
        commands in prop::collection::vec(command(), 1..250),
    ) {
        let (mut engine, mut publisher, mut builder) = (Engine::new(), Publisher::new(), BookBuilder::new());
        let (mut events, mut messages) = (Vec::new(), Vec::new());
        let mut next_seq = 1;
        for command in &commands {
            events.clear();
            messages.clear();
            engine.process(command, &mut events);
            publisher.publish(&events, &mut messages);

            let trades: Vec<_> = events
                .iter()
                .filter_map(|event| match *event {
                    Event::Trade { price, qty, taker_side, .. } => Some((price, qty, taker_side)),
                    _ => None,
                })
                .collect();
            let prints: Vec<_> = messages
                .iter()
                .filter_map(|message| match *message {
                    Message::Trade { price, qty, aggressor, .. } => Some((price, qty, aggressor)),
                    Message::Level { .. } => None,
                })
                .collect();
            prop_assert_eq!(prints, trades);
            check_order(&messages)?;

            for message in &messages {
                prop_assert_eq!(message.seq(), next_seq, "sequence numbers have no gaps");
                next_seq += 1;
                builder.apply(message).map_err(|gap| TestCaseError::fail(gap.to_string()))?;
            }
            prop_assert_eq!(builder.depth(usize::MAX), engine.depth(usize::MAX), "after {:?}", command);
            prop_assert_eq!(publisher.snapshot().depth, engine.depth(usize::MAX));
        }
    }

    #[test]
    fn a_lost_message_is_detected_and_repaired(
        commands in prop::collection::vec(command(), 1..250),
        lose in any::<Index>(),
    ) {
        // Run once to count the messages, then again losing one of them.
        let mut publisher = Publisher::new();
        let (mut engine, mut events, mut messages) = (Engine::new(), Vec::new(), Vec::new());
        for command in &commands {
            engine.process(command, &mut events);
            publisher.publish(&std::mem::take(&mut events), &mut messages);
        }
        if messages.is_empty() {
            return Ok(());
        }
        let lost = lose.index(messages.len()) as u64 + 1;

        let (mut engine, mut publisher, mut builder) = (Engine::new(), Publisher::new(), BookBuilder::new());
        let mut detected = false;
        for command in &commands {
            events.clear();
            messages.clear();
            engine.process(command, &mut events);
            publisher.publish(&events, &mut messages);
            for message in messages.iter().filter(|message| message.seq() != lost) {
                if let Err(gap) = builder.apply(message) {
                    prop_assert_eq!(gap.expected, lost);
                    detected = true;
                }
            }
            if let Err(gap) = builder.heartbeat(publisher.seq()) {
                prop_assert_eq!(gap.expected, lost);
                detected = true;
            }
            if !builder.is_synced() {
                builder.resync(&publisher.snapshot());
            }
            prop_assert_eq!(builder.depth(usize::MAX), engine.depth(usize::MAX), "after {:?}", command);
        }
        prop_assert!(detected, "message {} was lost silently", lost);
    }
}

/// A long synthetic session with deep books and sweeps of many levels.
#[test]
fn synthetic_session_stays_in_sync() {
    let mut flow = Flow::new(FlowConfig::seeded(7));
    let (mut engine, mut publisher, mut builder) =
        (Engine::new(), Publisher::new(), BookBuilder::new());
    let (mut events, mut messages) = (Vec::new(), Vec::new());
    let (mut trades, mut levels) = (0u64, 0u64);
    for n in 1..=40_000u32 {
        let command: Command = flow.next().expect("flow is endless");
        events.clear();
        messages.clear();
        engine.process(&command, &mut events);
        publisher.publish(&events, &mut messages);
        for message in &messages {
            match message {
                Message::Trade { .. } => trades += 1,
                Message::Level { .. } => levels += 1,
            }
            builder.apply(message).expect("no gaps on a lossless feed");
        }
        if n % 500 == 0 {
            assert_eq!(
                builder.depth(usize::MAX),
                engine.depth(usize::MAX),
                "after {n} commands"
            );
        }
    }
    assert_eq!(builder.depth(usize::MAX), engine.depth(usize::MAX));
    assert!(
        trades > 1_000 && levels > 10_000,
        "{trades} trades, {levels} level updates"
    );
}
