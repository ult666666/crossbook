//! Differential property tests.
//!
//! Random command streams run through the fast engine and the reference model. After
//! every command the two must emit identical events and hold identical books, the fast
//! engine's internal structure must pass `check_invariants`, and the events must conserve
//! quantity. The generators use a small id space, a narrow price band and few accounts so
//! that duplicates, unknown ids, crossing prices and self-trades are common. Sessions with
//! auctions add call phases, uncrosses and resumes at random points, and every uncross is
//! checked against the brute-force price determination and the allocation invariants.
//!
//! Set `PROPTEST_CASES` to change the number of streams (CI runs 4096).

mod common;

use common::{check_uncross, command, config, session_command};
use crossbook::reference::ReferenceEngine;
use crossbook::{CancelReason, Command, Engine, Event, Phase, Side};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

/// Checks that one command's events account for every lot it entered.
fn check_accounting(command: &Command, events: &[Event]) -> Result<(), TestCaseError> {
    let (id, entered, limit) = match (command, events.first()) {
        (Command::New(order), Some(&Event::Accepted { qty, .. })) => {
            (order.id, qty, order.kind.limit())
        }
        (
            &Command::Modify { id, price, .. },
            Some(&Event::Modified {
                qty,
                kept_priority: false,
                ..
            }),
        ) => (id, qty, Some(price)),
        (Command::StartCall { .. }, Some(Event::CallStarted { .. })) => {
            prop_assert!(
                events[1..].iter().all(|event| matches!(
                    event,
                    Event::Canceled {
                        reason: CancelReason::CallPhase,
                        ..
                    }
                )),
                "a call phase only removes post-only orders: {events:?}"
            );
            return Ok(());
        }
        (Command::Uncross, _) => {
            let ends_with_summary = matches!(events.last(), Some(Event::Uncrossed { .. }));
            prop_assert!(
                ends_with_summary,
                "an uncross ends with its summary: {:?}",
                events
            );
            return Ok(());
        }
        (Command::Resume, _) => {
            prop_assert_eq!(events.last(), Some(&Event::Resumed));
            let uncrossed = matches!(
                events[..],
                [Event::Resumed] | [.., Event::Uncrossed { .. }, Event::Resumed]
            );
            prop_assert!(
                uncrossed,
                "a resume uncrosses or does nothing else: {:?}",
                events
            );
            return Ok(());
        }
        _ => {
            prop_assert!(
                events.len() == 1,
                "a rejection or simple change emits one event"
            );
            return Ok(());
        }
    };
    let mut accounted = 0u64;
    let mut finished = false;
    for event in &events[1..] {
        prop_assert!(
            !finished,
            "events after the incoming order finished: {events:?}"
        );
        match *event {
            Event::Trade {
                taker,
                taker_side,
                price,
                qty,
                ..
            } => {
                prop_assert_eq!(taker, id);
                prop_assert!(qty > 0);
                if let Some(limit) = limit {
                    let respects_limit = match taker_side {
                        Side::Buy => price <= limit,
                        Side::Sell => price >= limit,
                    };
                    prop_assert!(respects_limit, "trade at {price} violates limit {limit}");
                }
                accounted += qty;
            }
            Event::Booked {
                id: booked, qty, ..
            }
            | Event::MarketBooked {
                id: booked, qty, ..
            } => {
                prop_assert_eq!(booked, id);
                accounted += qty;
                finished = true;
            }
            Event::Canceled {
                id: canceled,
                remaining,
                reason,
            } if canceled == id => {
                prop_assert!(reason != CancelReason::Requested);
                accounted += remaining;
                finished = true;
            }
            Event::Canceled { reason, .. } => prop_assert_eq!(reason, CancelReason::SelfTrade),
            other => return Err(TestCaseError::fail(format!("unexpected event {other:?}"))),
        }
    }
    prop_assert_eq!(accounted, entered, "quantity not conserved: {:?}", events);
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn fast_engine_matches_reference(commands in prop::collection::vec(command(), 1..250)) {
        let mut fast = Engine::new();
        let mut reference = ReferenceEngine::new();
        for command in &commands {
            let (mut fast_events, mut reference_events) = (Vec::new(), Vec::new());
            fast.process(command, &mut fast_events);
            reference.process(command, &mut reference_events);
            prop_assert_eq!(&fast_events, &reference_events, "after {:?}", command);
            fast.check_invariants().map_err(TestCaseError::fail)?;
            prop_assert_eq!(fast.snapshot(), reference.snapshot());
            check_accounting(command, &fast_events)?;
        }
        prop_assert_eq!(fast.depth(16), reference.depth(16));
        prop_assert_eq!(fast.len(), reference.len());
    }

    #[test]
    fn fast_engine_matches_reference_through_auctions(commands in prop::collection::vec(session_command(), 1..250)) {
        let mut fast = Engine::new();
        let mut reference = ReferenceEngine::new();
        for command in &commands {
            let (before, reference_price) = (fast.snapshot(), fast.reference_price());
            let (mut fast_events, mut reference_events) = (Vec::new(), Vec::new());
            fast.process(command, &mut fast_events);
            reference.process(command, &mut reference_events);
            prop_assert_eq!(&fast_events, &reference_events, "after {:?}", command);
            fast.check_invariants().map_err(TestCaseError::fail)?;
            prop_assert_eq!(fast.snapshot(), reference.snapshot());
            prop_assert_eq!(
                (fast.phase(), fast.reference_price(), fast.last_trade_price()),
                (reference.phase(), reference.reference_price(), reference.last_trade_price())
            );
            prop_assert_eq!(fast.indicative_uncross(), reference.indicative_uncross());
            prop_assert_eq!(fast.depth(16), reference.depth(16));
            for side in [Side::Buy, Side::Sell] {
                prop_assert_eq!(fast.market_orders(side), reference.market_orders(side));
            }
            check_accounting(command, &fast_events)?;
            check_uncross(&before, reference_price, &fast_events, &fast)?;
            if fast.phase() == Phase::Continuous {
                prop_assert_eq!(fast.indicative_uncross(), crossbook::UncrossSummary::default());
            }
        }
    }

    #[test]
    fn rejected_commands_leave_the_book_unchanged(commands in prop::collection::vec(session_command(), 1..120)) {
        let mut engine = Engine::new();
        for command in &commands {
            let before = engine.state_hash();
            let mut events = Vec::new();
            engine.process(command, &mut events);
            if matches!(events.as_slice(), [Event::Rejected { .. }]) {
                prop_assert_eq!(engine.state_hash(), before);
            }
        }
    }

    #[test]
    fn replaying_the_same_commands_is_deterministic(commands in prop::collection::vec(session_command(), 1..120)) {
        let run = |commands: &[Command]| {
            let mut engine = Engine::new();
            let mut events = Vec::new();
            for command in commands {
                engine.process(command, &mut events);
            }
            (events, engine.state_hash())
        };
        prop_assert_eq!(run(&commands), run(&commands));
    }
}
