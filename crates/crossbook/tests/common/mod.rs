//! Generators shared by the property tests.
//!
//! They use a small id space, a narrow price band and few accounts so that duplicates,
//! unknown ids, crossing prices and self-trades are common. Set `PROPTEST_CASES` to
//! change the number of cases (CI runs 4096).

#![allow(dead_code)] // Each test binary uses a different subset.

use crossbook::{Command, MAX_QTY, NewOrder, OrderKind, Side, StpMode, TimeInForce};
use proptest::prelude::*;

pub fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Buy), Just(Side::Sell)]
}

pub fn price() -> impl Strategy<Value = i64> {
    prop_oneof![40 => 95i64..=105, 1 => Just(0), 1 => Just(-3)]
}

pub fn qty() -> impl Strategy<Value = u64> {
    prop_oneof![40 => 1u64..=12, 1 => Just(0), 1 => Just(MAX_QTY), 1 => Just(MAX_QTY + 1)]
}

pub fn new_order() -> impl Strategy<Value = NewOrder> {
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

pub fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        6 => new_order().prop_map(Command::New),
        2 => (1u64..=30).prop_map(|id| Command::Cancel { id }),
        2 => (1u64..=30, price(), qty()).prop_map(|(id, price, qty)| Command::Modify { id, price, qty }),
    ]
}

pub fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}
