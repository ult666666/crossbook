//! Differential property tests.
//!
//! Random command streams run through the fast engine and the reference model. After
//! every command the two must emit identical events and hold identical books, the fast
//! engine's internal structure must pass `check_invariants`, and the events must conserve
//! quantity. The generators use a small id space, a narrow price band and few accounts so
//! that duplicates, unknown ids, crossing prices and self-trades are common.
//!
//! Set `PROPTEST_CASES` to change the number of streams (CI runs 4096).

use crossbook::reference::ReferenceEngine;
use crossbook::{
    CancelReason, Command, Engine, Event, MAX_QTY, NewOrder, OrderKind, Side, StpMode, TimeInForce,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Buy), Just(Side::Sell)]
}

fn price() -> impl Strategy<Value = i64> {
    prop_oneof![40 => 95i64..=105, 1 => Just(0), 1 => Just(-3)]
}

fn qty() -> impl Strategy<Value = u64> {
    prop_oneof![40 => 1u64..=12, 1 => Just(0), 1 => Just(MAX_QTY), 1 => Just(MAX_QTY + 1)]
}

fn new_order() -> impl Strategy<Value = NewOrder> {
    let kind = prop_oneof![9 => price().prop_map(OrderKind::Limit), 1 => Just(OrderKind::Market)];
    let tif = prop_oneof![6 => Just(TimeInForce::Gtc), 2 => Just(TimeInForce::Ioc), 2 => Just(TimeInForce::Fok)];
    let stp = prop_oneof![
        4 => Just(StpMode::Allow),
        1 => Just(StpMode::CancelNewest),
        1 => Just(StpMode::CancelOldest),
        1 => Just(StpMode::CancelBoth),
    ];
    (
        1u64..=30,
        1u32..=3,
        side(),
        kind,
        qty(),
        tif,
        prop::bool::weighted(0.15),
        stp,
    )
        .prop_map(
            |(id, account, side, kind, qty, tif, post_only, stp)| NewOrder {
                id,
                account,
                side,
                kind,
                qty,
                tif,
                post_only,
                stp,
            },
        )
}

fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        6 => new_order().prop_map(Command::New),
        2 => (1u64..=30).prop_map(|id| Command::Cancel { id }),
        2 => (1u64..=30, price(), qty()).prop_map(|(id, price, qty)| Command::Modify { id, price, qty }),
    ]
}

fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}

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
    fn rejected_commands_leave_the_book_unchanged(commands in prop::collection::vec(command(), 1..120)) {
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
    fn replaying_the_same_commands_is_deterministic(commands in prop::collection::vec(command(), 1..120)) {
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
