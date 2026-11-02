//! Market-data property tests.
//!
//! The feed is derived from events alone, so the strongest check is end to end: after
//! every command, a subscriber that applied the messages must hold exactly the engine's
//! book, level by level, and the trade prints, auction results and phase changes must
//! mirror the events. The random sessions include call phases, where the book may cross
//! and market orders wait outside every level, and uncrosses that empty many levels at
//! once. A subscriber that loses one message must notice by the end of that command, from
//! a later message or the heartbeat, and must hold the engine's book again after
//! resynchronizing.

mod common;

use common::{config, session_command};
use crossbook::flow::{Flow, FlowConfig, Schedule};
use crossbook::market_data::{BookBuilder, Message, Publisher};
use crossbook::{Command, Engine, Event, Phase, Side, UncrossSummary};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;

/// What the feed should announce for one command, in event order.
#[derive(Debug, PartialEq, Eq)]
enum Announced {
    Trade(i64, u64, Side),
    Auction(UncrossSummary),
    Status(Phase, Option<i64>),
}

fn expected(events: &[Event]) -> Vec<Announced> {
    events
        .iter()
        .filter_map(|event| match *event {
            Event::Trade {
                price,
                qty,
                taker_side,
                ..
            } => Some(Announced::Trade(price, qty, taker_side)),
            Event::Uncrossed {
                price,
                volume,
                imbalance_side,
                imbalance,
            } => Some(Announced::Auction(UncrossSummary {
                price,
                volume,
                imbalance_side,
                imbalance,
            })),
            Event::CallStarted { reference } => Some(Announced::Status(Phase::Call, reference)),
            Event::Resumed => Some(Announced::Status(Phase::Continuous, None)),
            _ => None,
        })
        .collect()
}

fn announced(messages: &[Message]) -> Vec<Announced> {
    messages
        .iter()
        .filter_map(|message| match *message {
            Message::Trade {
                price,
                qty,
                aggressor,
                ..
            } => Some(Announced::Trade(price, qty, aggressor)),
            Message::Auction { result, .. } => Some(Announced::Auction(result)),
            Message::Status {
                phase, reference, ..
            } => Some(Announced::Status(phase, reference)),
            Message::Level { .. } => None,
        })
        .collect()
}

/// Prints and status first, in event order; then level updates, bids before asks, best first.
fn check_order(messages: &[Message]) -> Result<(), TestCaseError> {
    let first_level = messages
        .iter()
        .position(|message| matches!(message, Message::Level { .. }))
        .unwrap_or(messages.len());
    prop_assert!(
        messages[first_level..]
            .iter()
            .all(|message| matches!(message, Message::Level { .. })),
        "a print or status message after a level update: {:?}",
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
            _ => unreachable!(),
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
        commands in prop::collection::vec(session_command(), 1..250),
    ) {
        let (mut engine, mut publisher, mut builder) = (Engine::new(), Publisher::new(), BookBuilder::new());
        let (mut events, mut messages) = (Vec::new(), Vec::new());
        let mut next_seq = 1;
        for command in &commands {
            events.clear();
            messages.clear();
            engine.process(command, &mut events);
            publisher.publish(&events, &mut messages);
            prop_assert_eq!(announced(&messages), expected(&events));
            check_order(&messages)?;

            for message in &messages {
                prop_assert_eq!(message.seq(), next_seq, "sequence numbers have no gaps");
                next_seq += 1;
                builder.apply(message).map_err(|gap| TestCaseError::fail(gap.to_string()))?;
            }
            prop_assert_eq!(builder.depth(usize::MAX), engine.depth(usize::MAX), "after {:?}", command);
            prop_assert_eq!(builder.phase(), engine.phase());
            let snapshot = publisher.snapshot();
            prop_assert_eq!(snapshot.depth, engine.depth(usize::MAX));
            prop_assert_eq!(snapshot.phase, engine.phase());
        }
    }

    #[test]
    fn a_lost_message_is_detected_and_repaired(
        commands in prop::collection::vec(session_command(), 1..250),
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
            prop_assert_eq!(builder.phase(), engine.phase());
        }
        prop_assert!(detected, "message {} was lost silently", lost);
    }
}

/// Long synthetic sessions with deep books and sweeps of many levels: continuous trading
/// only, and two trading days with opening and closing auctions.
#[test]
fn synthetic_session_stays_in_sync() {
    let schedule = Schedule {
        opening_call: 1_500,
        continuous: 15_000,
        closing_call: 1_500,
    };
    let two_days = 2 * schedule.day_len();
    for (session, commands) in [(None, 40_000), (Some(schedule), two_days)] {
        let mut flow = Flow::new(FlowConfig {
            session,
            ..FlowConfig::seeded(7)
        });
        let (mut engine, mut publisher, mut builder) =
            (Engine::new(), Publisher::new(), BookBuilder::new());
        let (mut events, mut messages) = (Vec::new(), Vec::new());
        let (mut trades, mut levels, mut auctions) = (0u64, 0u64, 0u64);
        for n in 1..=commands {
            let command: Command = flow.next().expect("flow is endless");
            events.clear();
            messages.clear();
            engine.process(&command, &mut events);
            publisher.publish(&events, &mut messages);
            for message in &messages {
                match message {
                    Message::Trade { .. } => trades += 1,
                    Message::Level { .. } => levels += 1,
                    Message::Auction { result, .. } => auctions += u64::from(result.volume > 0),
                    Message::Status { .. } => {}
                }
                builder.apply(message).expect("no gaps on a lossless feed");
            }
            if n % 500 == 0 || matches!(command, Command::Uncross) {
                assert_eq!(
                    builder.depth(usize::MAX),
                    engine.depth(usize::MAX),
                    "after {n} commands"
                );
            }
        }
        assert_eq!(builder.depth(usize::MAX), engine.depth(usize::MAX));
        assert_eq!(builder.phase(), engine.phase());
        assert!(
            trades > 1_000 && levels > 10_000,
            "{trades} trades, {levels} level updates"
        );
        let expected_auctions = if session.is_some() { 4 } else { 0 };
        assert_eq!(auctions, expected_auctions, "uncrosses that executed");
    }
}
