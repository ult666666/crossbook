//! Generators shared by the property tests.
//!
//! They use a small id space, a narrow price band and few accounts so that duplicates,
//! unknown ids, crossing prices and self-trades are common. `session_command` adds call
//! phases, uncrosses and market orders that wait for them. Set `PROPTEST_CASES` to
//! change the number of cases (CI runs 4096).

#![allow(dead_code)] // Each test binary uses a different subset.

use crossbook::reference::{AuctionOrder, auction_price};
use crossbook::{
    CancelReason, Command, Engine, Event, MAX_QTY, NewOrder, OrderId, OrderKind, OrderView, Price,
    Qty, Side, StpMode, TimeInForce, UncrossSummary,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

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

/// Starts a call phase, sometimes with a reference price (including invalid ones).
pub fn start_call() -> impl Strategy<Value = Command> {
    prop_oneof![
        2 => Just(None),
        3 => (95i64..=105).prop_map(Some),
        1 => Just(Some(0)),
    ]
    .prop_map(|reference| Command::StartCall { reference })
}

/// A command for a session with auctions: everything `command` produces, plus phase
/// commands about one time in eleven. In a call phase, about one in three of the new
/// orders is a GTC market order or a plain GTC limit that may cross the book.
pub fn session_command() -> impl Strategy<Value = Command> {
    prop_oneof![
        10 => command(),
        2 => call_order().prop_map(Command::New),
        2 => start_call(),
        1 => Just(Command::Uncross),
        2 => Just(Command::Resume),
    ]
}

/// A command that keeps a call phase going: no `Resume`.
pub fn call_command() -> impl Strategy<Value = Command> {
    prop_oneof![
        10 => command(),
        4 => call_order().prop_map(Command::New),
        1 => start_call(),
        1 => Just(Command::Uncross),
    ]
}

/// A GTC order that a call phase accepts: a limit order or a market order.
pub fn call_order() -> impl Strategy<Value = NewOrder> {
    let kind =
        prop_oneof![3 => (95i64..=105).prop_map(OrderKind::Limit), 1 => Just(OrderKind::Market)];
    (1u64..=30, 1u32..=3, side(), kind, 1u64..=12).prop_map(|(id, account, side, kind, qty)| {
        NewOrder {
            id,
            account,
            side,
            kind,
            qty,
            tif: TimeInForce::Gtc,
            post_only: false,
            stp: StpMode::Allow,
        }
    })
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

/// Checks one command's uncross, if it ran one, against the book just before it.
///
/// `before` is `Engine::snapshot()` before the command: each side lists its orders in
/// uncross priority (market orders, then best price, then time). The checks: the summary
/// equals the brute-force price determination; every fill is at the clearing price and no
/// worse than the order's limit; the fills add up to the volume on both sides; each side
/// fills a prefix of its priority order, so only the last order touched can be partial; the
/// side without a surplus fills completely and the other side keeps exactly the surplus;
/// market orders that did not fill are canceled; and the book is no longer crossed.
pub fn check_uncross(
    before: &[OrderView],
    reference: Option<Price>,
    events: &[Event],
    after: &Engine,
) -> Result<(), TestCaseError> {
    use std::collections::HashMap;

    let Some(end) = events
        .iter()
        .position(|event| matches!(event, Event::Uncrossed { .. }))
    else {
        return Ok(());
    };
    let Event::Uncrossed {
        price,
        volume,
        imbalance_side,
        imbalance,
    } = events[end]
    else {
        unreachable!()
    };
    let summary = UncrossSummary {
        price,
        volume,
        imbalance_side,
        imbalance,
    };
    let orders: Vec<AuctionOrder> = before
        .iter()
        .map(|order| AuctionOrder {
            side: order.side,
            limit: (!order.is_market()).then_some(order.price),
            qty: order.remaining,
        })
        .collect();
    let (expected, _) = auction_price(&orders, reference);
    prop_assert_eq!(
        summary,
        expected,
        "uncross summary differs from the brute force"
    );

    let by_id: HashMap<OrderId, OrderView> =
        before.iter().map(|order| (order.id, *order)).collect();
    let mut filled: HashMap<OrderId, Qty> = HashMap::new();
    let (mut traded, mut first_cancel) = (0, end);
    for (index, event) in events[..end].iter().enumerate() {
        match *event {
            Event::AuctionTrade {
                buy,
                sell,
                price: at,
                qty,
                buy_remaining,
                sell_remaining,
            } => {
                prop_assert!(first_cancel == end, "a trade after a cancel: {events:?}");
                prop_assert_eq!(
                    Some(at),
                    price,
                    "auction trade away from the clearing price"
                );
                prop_assert!(qty > 0);
                traded += qty;
                for (id, side, remaining) in [
                    (buy, Side::Buy, buy_remaining),
                    (sell, Side::Sell, sell_remaining),
                ] {
                    let order = by_id.get(&id).copied();
                    prop_assert!(
                        order.is_some_and(|o| o.side == side),
                        "order {} did not rest as a {} order",
                        id,
                        side
                    );
                    let order = order.unwrap();
                    let worse = !order.is_market()
                        && match side {
                            Side::Buy => order.price < at,
                            Side::Sell => order.price > at,
                        };
                    prop_assert!(
                        !worse,
                        "order {} filled at {} beyond its limit {}",
                        id,
                        at,
                        order.price
                    );
                    let total = filled.entry(id).or_insert(0);
                    *total += qty;
                    prop_assert!(*total <= order.remaining, "order {} overfilled", id);
                    prop_assert_eq!(
                        remaining,
                        order.remaining - *total,
                        "order {} remaining",
                        id
                    );
                }
            }
            Event::Canceled {
                id,
                remaining,
                reason: CancelReason::Unfilled,
            } => {
                first_cancel = first_cancel.min(index);
                let order = by_id.get(&id).copied();
                prop_assert!(
                    order.is_some_and(|o| o.is_market()),
                    "canceled {} is not a waiting market order",
                    id
                );
                let open = order.unwrap().remaining - filled.get(&id).copied().unwrap_or(0);
                prop_assert_eq!(remaining, open, "market order {} canceled quantity", id);
            }
            // A `Resume` that uncrosses lists nothing else before the summary.
            ref other => {
                return Err(TestCaseError::fail(format!(
                    "unexpected event {other:?} in an uncross"
                )));
            }
        }
    }
    prop_assert_eq!(traded, volume, "trades do not add up to the volume");

    for side in [Side::Buy, Side::Sell] {
        let queue: Vec<&OrderView> = before.iter().filter(|order| order.side == side).collect();
        let fills: Vec<Qty> = queue
            .iter()
            .map(|order| filled.get(&order.id).copied().unwrap_or(0))
            .collect();
        prop_assert_eq!(
            fills.iter().sum::<Qty>(),
            volume,
            "{} fills do not add up",
            side
        );
        // A prefix fills: once an order is not filled in full, nothing after it fills.
        let short = queue
            .iter()
            .zip(&fills)
            .position(|(order, &fill)| fill < order.remaining);
        if let Some(short) = short {
            prop_assert!(
                fills[short + 1..].iter().all(|&fill| fill == 0),
                "{} fills skip priority: {:?}",
                side,
                fills
            );
        }
        let eligible_left: Qty = queue
            .iter()
            .zip(&fills)
            .filter(|(order, _)| price.is_some_and(|at| order.is_market() || crosses_at(order, at)))
            .map(|(order, &fill)| order.remaining - fill)
            .sum();
        let surplus = if imbalance_side == Some(side) {
            imbalance
        } else {
            0
        };
        if price.is_some() {
            prop_assert_eq!(
                eligible_left,
                surplus,
                "{} quantity left at the clearing price",
                side
            );
        }
    }

    prop_assert!(
        after.market_orders(Side::Buy).is_empty() && after.market_orders(Side::Sell).is_empty()
    );
    if let (Some(bid), Some(ask)) = (after.best_bid(), after.best_ask()) {
        prop_assert!(
            bid.price < ask.price,
            "still crossed after the uncross: {} >= {}",
            bid.price,
            ask.price
        );
    }
    Ok(())
}

/// True when a limit order would trade at `price` in an auction.
fn crosses_at(order: &OrderView, price: Price) -> bool {
    match order.side {
        Side::Buy => order.price >= price,
        Side::Sell => order.price <= price,
    }
}
