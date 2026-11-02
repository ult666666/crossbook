//! Auction price determination against brute force.
//!
//! The engine finds the clearing price in one pass over its price levels. The reference
//! model's `auction_price` instead evaluates every distinct limit price and applies the
//! rules literally, keeping the tied candidates after each one. Small random books (five
//! price levels, one to four lots per order, some market orders) produce ties at every
//! rule, so the two must agree on the price, volume and surplus, and each uncross must pass
//! the allocation checks in `common::check_uncross`.

mod common;

use std::collections::HashMap;

use common::{check_uncross, config};
use crossbook::flow::SplitMix64;
use crossbook::reference::{AuctionOrder, Decided, ReferenceEngine, auction_price};
use crossbook::{Command, Engine, NewOrder, OrderKind, Side, StpMode, TimeInForce};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

fn order() -> impl Strategy<Value = AuctionOrder> {
    let limit = prop_oneof![5 => (98i64..=102).prop_map(Some), 1 => Just(None)];
    (
        prop_oneof![Just(Side::Buy), Just(Side::Sell)],
        limit,
        1u64..=4,
    )
        .prop_map(|(side, limit, qty)| AuctionOrder { side, limit, qty })
}

/// Enters `orders` in a call phase with `reference`, checks the indicative price against
/// brute force, uncrosses and checks the allocation. Returns the rule that decided.
fn check_book(orders: &[AuctionOrder], reference: Option<i64>) -> Result<Decided, TestCaseError> {
    let (mut engine, mut model) = (Engine::new(), ReferenceEngine::new());
    let mut events = Vec::new();
    let mut commands = vec![Command::StartCall { reference }];
    for (id, order) in (1u64..).zip(orders) {
        commands.push(Command::New(NewOrder {
            id,
            account: 1,
            side: order.side,
            kind: order.limit.map_or(OrderKind::Market, OrderKind::Limit),
            qty: order.qty,
            tif: TimeInForce::Gtc,
            post_only: false,
            stp: StpMode::CancelBoth,
        }));
    }
    for command in &commands {
        engine.process(command, &mut events);
        model.process(command, &mut Vec::new());
    }
    let (expected, decided) = auction_price(orders, reference);
    prop_assert_eq!(engine.indicative_uncross(), expected, "{:?}", orders);
    prop_assert_eq!(model.indicative_uncross(), expected);

    let before = engine.snapshot();
    let (mut fast, mut slow) = (Vec::new(), Vec::new());
    engine.process(&Command::Uncross, &mut fast);
    model.process(&Command::Uncross, &mut slow);
    prop_assert_eq!(&fast, &slow);
    check_uncross(&before, reference, &fast, &engine)?;
    engine.check_invariants().map_err(TestCaseError::fail)?;
    prop_assert_eq!(engine.snapshot(), model.snapshot());
    Ok(decided)
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn engine_price_equals_brute_force(
        orders in prop::collection::vec(order(), 0..14),
        reference in prop_oneof![1 => Just(None), 3 => (96i64..=104).prop_map(Some)],
    ) {
        check_book(&orders, reference)?;
    }
}

/// 20,000 seeded books: the engine agrees with brute force on every one, and every rule,
/// including the final tie-break, decides some of them.
#[test]
fn every_rule_decides_some_books() {
    let mut rng = SplitMix64::new(2026);
    let mut decided: HashMap<Decided, u32> = HashMap::new();
    for _ in 0..20_000 {
        // One book in twenty holds market orders only.
        let market_only = rng.below(20) == 0;
        let orders: Vec<AuctionOrder> = (0..rng.below(13))
            .map(|_| AuctionOrder {
                side: if rng.below(2) == 0 {
                    Side::Buy
                } else {
                    Side::Sell
                },
                limit: (!market_only && rng.below(6) > 0).then(|| 98 + rng.below(5) as i64),
                qty: 1 + rng.below(4),
            })
            .collect();
        let reference = (rng.below(4) > 0).then(|| 96 + rng.below(9) as i64);
        let rule = check_book(&orders, reference).unwrap_or_else(|error| panic!("{error}"));
        *decided.entry(rule).or_default() += 1;
    }
    let mut counts: Vec<_> = decided
        .iter()
        .map(|(rule, count)| (format!("{rule:?}"), *count))
        .collect();
    counts.sort();
    println!("books decided by each rule: {counts:?}");
    for rule in [
        Decided::NoTrade,
        Decided::MarketOnly,
        Decided::Volume,
        Decided::Surplus,
        Decided::MarketPressure,
        Decided::Reference,
        Decided::LowestPrice,
    ] {
        let count = decided.get(&rule).copied().unwrap_or(0);
        assert!(count >= 100, "only {count} books decided by {rule:?}");
    }
}
