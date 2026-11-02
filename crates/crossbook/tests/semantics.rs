//! Scenario tests: one test per rule in docs/SEMANTICS.md.
//!
//! Every scenario runs on both the fast engine and the reference model, checks that they
//! emit the same events, and checks the fast engine's structural invariants after each
//! command. The assertions then pin the exact expected events.

use crossbook::reference::ReferenceEngine;
use crossbook::{
    CancelReason, Command, Engine, Event, NewOrder, Phase, RejectReason, Side, StpMode,
    TimeInForce, UncrossSummary,
};

use Side::{Buy, Sell};

struct Books {
    fast: Engine,
    reference: ReferenceEngine,
}

impl Books {
    fn new() -> Self {
        Self {
            fast: Engine::new(),
            reference: ReferenceEngine::new(),
        }
    }

    /// Runs one command on both engines and returns its events.
    fn run(&mut self, command: Command) -> Vec<Event> {
        let (mut fast, mut reference) = (Vec::new(), Vec::new());
        self.fast.process(&command, &mut fast);
        self.reference.process(&command, &mut reference);
        assert_eq!(fast, reference, "engines disagree on {command:?}");
        self.fast.check_invariants().unwrap();
        assert_eq!(self.fast.snapshot(), self.reference.snapshot());
        assert_eq!(self.fast.phase(), self.reference.phase());
        assert_eq!(
            self.fast.indicative_uncross(),
            self.reference.indicative_uncross()
        );
        fast
    }

    fn start_call(&mut self, reference: Option<i64>) -> Vec<Event> {
        self.run(Command::StartCall { reference })
    }

    fn market(&mut self, id: u64, side: Side, qty: u64) -> Vec<Event> {
        self.new_order(NewOrder::market(id, side, qty).tif(TimeInForce::Gtc))
    }

    fn new_order(&mut self, order: NewOrder) -> Vec<Event> {
        self.run(Command::New(order))
    }

    fn limit(&mut self, id: u64, side: Side, price: i64, qty: u64) -> Vec<Event> {
        self.new_order(NewOrder::limit(id, side, price, qty))
    }
}

fn trade(
    taker: u64,
    maker: u64,
    side: Side,
    price: i64,
    qty: u64,
    taker_left: u64,
    maker_left: u64,
) -> Event {
    Event::Trade {
        taker,
        maker,
        taker_side: side,
        price,
        qty,
        taker_remaining: taker_left,
        maker_remaining: maker_left,
    }
}

fn accepted(id: u64, side: Side, price: Option<i64>, qty: u64) -> Event {
    Event::Accepted {
        id,
        side,
        price,
        qty,
    }
}

fn booked(id: u64, side: Side, price: i64, qty: u64) -> Event {
    Event::Booked {
        id,
        side,
        price,
        qty,
    }
}

fn canceled(id: u64, remaining: u64, reason: CancelReason) -> Event {
    Event::Canceled {
        id,
        remaining,
        reason,
    }
}

fn rejected(id: u64, reason: RejectReason) -> Event {
    Event::Rejected { id, reason }
}

#[test]
fn passive_order_rests() {
    let mut books = Books::new();
    assert_eq!(
        books.limit(1, Buy, 99, 10),
        [accepted(1, Buy, Some(99), 10), booked(1, Buy, 99, 10)]
    );
    assert_eq!(
        books.fast.best_bid().map(|level| (level.price, level.qty)),
        Some((99, 10))
    );
    assert_eq!(books.fast.best_ask(), None);
    assert!(!books.fast.is_empty());
    assert_eq!(books.fast.len(), 1);
}

#[test]
fn better_price_trades_first_then_earlier_order() {
    let mut books = Books::new();
    books.limit(1, Sell, 101, 5);
    books.limit(2, Sell, 100, 5);
    books.limit(3, Sell, 100, 5);
    let events = books.limit(4, Buy, 101, 12);
    assert_eq!(
        events,
        [
            accepted(4, Buy, Some(101), 12),
            trade(4, 2, Buy, 100, 5, 7, 0),
            trade(4, 3, Buy, 100, 5, 2, 0),
            trade(4, 1, Buy, 101, 2, 0, 3),
        ]
    );
    assert_eq!(books.fast.order(1).map(|order| order.remaining), Some(3));
}

#[test]
fn trade_executes_at_the_resting_price() {
    let mut books = Books::new();
    books.limit(1, Buy, 100, 4);
    let events = books.limit(2, Sell, 95, 4);
    assert_eq!(events[1], trade(2, 1, Sell, 100, 4, 0, 0));
}

#[test]
fn unfilled_limit_remainder_rests_at_its_limit() {
    let mut books = Books::new();
    books.limit(1, Sell, 100, 3);
    let events = books.limit(2, Buy, 102, 10);
    assert_eq!(events.last(), Some(&booked(2, Buy, 102, 7)));
    assert_eq!(books.fast.spread(), None);
    assert_eq!(books.fast.best_bid().map(|level| level.price), Some(102));
}

#[test]
fn market_order_sweeps_and_cancels_the_rest() {
    let mut books = Books::new();
    books.limit(1, Sell, 100, 2);
    books.limit(2, Sell, 105, 2);
    let events = books.new_order(NewOrder::market(3, Buy, 10));
    assert_eq!(
        events,
        [
            accepted(3, Buy, None, 10),
            trade(3, 1, Buy, 100, 2, 8, 0),
            trade(3, 2, Buy, 105, 2, 6, 0),
            canceled(3, 6, CancelReason::Unfilled),
        ]
    );
    assert!(books.fast.is_empty());
}

#[test]
fn market_order_into_empty_book_is_accepted_then_canceled() {
    let mut books = Books::new();
    let events = books.new_order(NewOrder::market(1, Sell, 5));
    assert_eq!(
        events,
        [
            accepted(1, Sell, None, 5),
            canceled(1, 5, CancelReason::Unfilled)
        ]
    );
}

#[test]
fn ioc_never_rests() {
    let mut books = Books::new();
    books.limit(1, Buy, 100, 3);
    let events = books.new_order(NewOrder::limit(2, Sell, 99, 5).tif(TimeInForce::Ioc));
    assert_eq!(
        events,
        [
            accepted(2, Sell, Some(99), 5),
            trade(2, 1, Sell, 100, 3, 2, 0),
            canceled(2, 2, CancelReason::Unfilled)
        ]
    );
    assert!(books.fast.is_empty());
}

#[test]
fn fok_fills_completely_or_not_at_all() {
    let mut books = Books::new();
    books.limit(1, Sell, 100, 4);
    books.limit(2, Sell, 101, 4);

    let too_big = books.new_order(NewOrder::limit(3, Buy, 101, 9).tif(TimeInForce::Fok));
    assert_eq!(too_big, [rejected(3, RejectReason::FillOrKillUnfillable)]);
    assert_eq!(
        books.fast.len(),
        2,
        "a rejected FOK must not touch the book"
    );

    let price_limited = books.new_order(NewOrder::limit(4, Buy, 100, 5).tif(TimeInForce::Fok));
    assert_eq!(
        price_limited,
        [rejected(4, RejectReason::FillOrKillUnfillable)]
    );

    let fits = books.new_order(NewOrder::limit(5, Buy, 101, 8).tif(TimeInForce::Fok));
    assert_eq!(
        fits,
        [
            accepted(5, Buy, Some(101), 8),
            trade(5, 1, Buy, 100, 4, 4, 0),
            trade(5, 2, Buy, 101, 4, 0, 0)
        ]
    );
}

#[test]
fn fok_market_order() {
    let mut books = Books::new();
    books.limit(1, Buy, 100, 4);
    let events = books.new_order(NewOrder::market(2, Sell, 5).tif(TimeInForce::Fok));
    assert_eq!(events, [rejected(2, RejectReason::FillOrKillUnfillable)]);
    let events = books.new_order(NewOrder::market(3, Sell, 4).tif(TimeInForce::Fok));
    assert_eq!(
        events,
        [accepted(3, Sell, None, 4), trade(3, 1, Sell, 100, 4, 0, 0)]
    );
}

#[test]
fn post_only_rejects_instead_of_crossing() {
    let mut books = Books::new();
    books.limit(1, Sell, 100, 5);
    let crossing = books.new_order(NewOrder::limit(2, Buy, 100, 5).post_only());
    assert_eq!(crossing, [rejected(2, RejectReason::PostOnlyWouldCross)]);
    let passive = books.new_order(NewOrder::limit(3, Buy, 99, 5).post_only());
    assert_eq!(
        passive,
        [accepted(3, Buy, Some(99), 5), booked(3, Buy, 99, 5)]
    );
}

#[test]
fn static_validation_runs_in_documented_order() {
    let mut books = Books::new();
    let zero = books.new_order(NewOrder::limit(1, Buy, -5, 0));
    assert_eq!(zero, [rejected(1, RejectReason::InvalidQuantity)]);
    let too_large = books.new_order(NewOrder::limit(1, Buy, 10, crossbook::MAX_QTY + 1));
    assert_eq!(too_large, [rejected(1, RejectReason::InvalidQuantity)]);
    let price = books.new_order(NewOrder::limit(1, Buy, 0, 5));
    assert_eq!(price, [rejected(1, RejectReason::InvalidPrice)]);
    let gtc_market = books.new_order(NewOrder::market(1, Buy, 5).tif(TimeInForce::Gtc));
    assert_eq!(
        gtc_market,
        [rejected(1, RejectReason::MarketOrderCannotRest)]
    );
    let ioc_post = books.new_order(
        NewOrder::limit(1, Buy, 10, 5)
            .tif(TimeInForce::Ioc)
            .post_only(),
    );
    assert_eq!(
        ioc_post,
        [rejected(1, RejectReason::PostOnlyRequiresGtcLimit)]
    );
    let market_post = books.new_order(NewOrder::market(1, Buy, 5).post_only());
    assert_eq!(
        market_post,
        [rejected(1, RejectReason::PostOnlyRequiresGtcLimit)]
    );
    assert!(books.fast.is_empty());
}

#[test]
fn largest_quantity_is_accepted() {
    let mut books = Books::new();
    let events = books.limit(1, Buy, 10, crossbook::MAX_QTY);
    assert_eq!(
        events,
        [
            accepted(1, Buy, Some(10), crossbook::MAX_QTY),
            booked(1, Buy, 10, crossbook::MAX_QTY)
        ]
    );
}

#[test]
fn ids_must_be_unique_among_live_orders_only() {
    let mut books = Books::new();
    books.limit(1, Buy, 100, 5);
    assert_eq!(
        books.limit(1, Sell, 105, 5),
        [rejected(1, RejectReason::DuplicateOrderId)]
    );
    books.run(Command::Cancel { id: 1 });
    assert_eq!(
        books.limit(1, Sell, 105, 5)[0],
        accepted(1, Sell, Some(105), 5)
    );
}

#[test]
fn cancel_removes_open_quantity() {
    let mut books = Books::new();
    books.limit(1, Buy, 100, 5);
    books.limit(2, Buy, 100, 7);
    assert_eq!(
        books.run(Command::Cancel { id: 1 }),
        [canceled(1, 5, CancelReason::Requested)]
    );
    assert_eq!(
        books.fast.best_bid().map(|level| (level.qty, level.orders)),
        Some((7, 1))
    );
    assert_eq!(
        books.run(Command::Cancel { id: 1 }),
        [rejected(1, RejectReason::UnknownOrder)]
    );
}

#[test]
fn reducing_quantity_keeps_priority() {
    let mut books = Books::new();
    books.limit(1, Sell, 100, 10);
    books.limit(2, Sell, 100, 10);
    let events = books.run(Command::Modify {
        id: 1,
        price: 100,
        qty: 4,
    });
    assert_eq!(
        events,
        [Event::Modified {
            id: 1,
            price: 100,
            qty: 4,
            kept_priority: true
        }]
    );
    let fill = books.limit(3, Buy, 100, 4);
    assert_eq!(
        fill[1],
        trade(3, 1, Buy, 100, 4, 0, 0),
        "order 1 is still first in the queue"
    );
}

#[test]
fn increasing_quantity_loses_priority() {
    let mut books = Books::new();
    books.limit(1, Sell, 100, 10);
    books.limit(2, Sell, 100, 10);
    let events = books.run(Command::Modify {
        id: 1,
        price: 100,
        qty: 12,
    });
    assert_eq!(
        events,
        [
            Event::Modified {
                id: 1,
                price: 100,
                qty: 12,
                kept_priority: false
            },
            booked(1, Sell, 100, 12)
        ]
    );
    let queue: Vec<_> = books
        .fast
        .queue(Sell, 100)
        .iter()
        .map(|order| order.id)
        .collect();
    assert_eq!(queue, [2, 1]);
}

#[test]
fn repricing_through_the_market_trades() {
    let mut books = Books::new();
    books.limit(1, Buy, 98, 5);
    books.limit(2, Sell, 100, 3);
    let events = books.run(Command::Modify {
        id: 1,
        price: 100,
        qty: 5,
    });
    assert_eq!(
        events,
        [
            Event::Modified {
                id: 1,
                price: 100,
                qty: 5,
                kept_priority: false
            },
            trade(1, 2, Buy, 100, 3, 2, 0),
            booked(1, Buy, 100, 2),
        ]
    );
}

#[test]
fn post_only_modify_that_would_cross_is_rejected_and_order_kept() {
    let mut books = Books::new();
    books.new_order(NewOrder::limit(1, Buy, 98, 5).post_only());
    books.limit(2, Sell, 100, 3);
    let events = books.run(Command::Modify {
        id: 1,
        price: 101,
        qty: 5,
    });
    assert_eq!(events, [rejected(1, RejectReason::PostOnlyWouldCross)]);
    assert_eq!(
        books
            .fast
            .order(1)
            .map(|order| (order.price, order.remaining)),
        Some((98, 5))
    );
}

#[test]
fn modify_validation() {
    let mut books = Books::new();
    books.limit(1, Buy, 98, 5);
    assert_eq!(
        books.run(Command::Modify {
            id: 9,
            price: 98,
            qty: 0
        }),
        [rejected(9, RejectReason::UnknownOrder)]
    );
    assert_eq!(
        books.run(Command::Modify {
            id: 1,
            price: 98,
            qty: 0
        }),
        [rejected(1, RejectReason::InvalidQuantity)]
    );
    assert_eq!(
        books.run(Command::Modify {
            id: 1,
            price: 0,
            qty: 2
        }),
        [rejected(1, RejectReason::InvalidPrice)]
    );
}

#[test]
fn stp_cancel_newest_stops_the_incoming_order() {
    let mut books = Books::new();
    books.new_order(NewOrder::limit(1, Sell, 100, 5).account(7));
    books.new_order(NewOrder::limit(2, Sell, 101, 5).account(8));
    let events = books.new_order(
        NewOrder::limit(3, Buy, 101, 8)
            .account(7)
            .stp(StpMode::CancelNewest),
    );
    assert_eq!(
        events,
        [
            accepted(3, Buy, Some(101), 8),
            canceled(3, 8, CancelReason::SelfTrade)
        ]
    );
    assert_eq!(books.fast.len(), 2, "resting orders are untouched");
}

#[test]
fn stp_cancel_oldest_removes_own_resting_order_and_keeps_matching() {
    let mut books = Books::new();
    books.new_order(NewOrder::limit(1, Sell, 100, 5).account(7));
    books.new_order(NewOrder::limit(2, Sell, 101, 5).account(8));
    let events = books.new_order(
        NewOrder::limit(3, Buy, 101, 8)
            .account(7)
            .stp(StpMode::CancelOldest),
    );
    assert_eq!(
        events,
        [
            accepted(3, Buy, Some(101), 8),
            canceled(1, 5, CancelReason::SelfTrade),
            trade(3, 2, Buy, 101, 5, 3, 0),
            booked(3, Buy, 101, 3),
        ]
    );
}

#[test]
fn stp_cancel_both() {
    let mut books = Books::new();
    books.new_order(NewOrder::limit(1, Sell, 100, 5).account(7));
    books.new_order(NewOrder::limit(2, Sell, 100, 5).account(8));
    let events = books.new_order(
        NewOrder::limit(3, Buy, 100, 8)
            .account(7)
            .stp(StpMode::CancelBoth),
    );
    assert_eq!(
        events,
        [
            accepted(3, Buy, Some(100), 8),
            canceled(1, 5, CancelReason::SelfTrade),
            canceled(3, 8, CancelReason::SelfTrade),
        ]
    );
    assert_eq!(books.fast.order(2).map(|order| order.remaining), Some(5));
}

#[test]
fn stp_allow_permits_self_trade() {
    let mut books = Books::new();
    books.new_order(NewOrder::limit(1, Sell, 100, 5).account(7));
    let events = books.new_order(NewOrder::limit(2, Buy, 100, 5).account(7));
    assert_eq!(events[1], trade(2, 1, Buy, 100, 5, 0, 0));
}

#[test]
fn stp_only_triggers_after_better_liquidity_is_used() {
    let mut books = Books::new();
    books.new_order(NewOrder::limit(1, Sell, 100, 2).account(8));
    books.new_order(NewOrder::limit(2, Sell, 100, 5).account(7));
    let events = books.new_order(
        NewOrder::limit(3, Buy, 100, 6)
            .account(7)
            .stp(StpMode::CancelNewest),
    );
    assert_eq!(
        events,
        [
            accepted(3, Buy, Some(100), 6),
            trade(3, 1, Buy, 100, 2, 4, 0),
            canceled(3, 4, CancelReason::SelfTrade)
        ]
    );
}

#[test]
fn fok_counts_only_liquidity_stp_would_leave() {
    let mut books = Books::new();
    books.new_order(NewOrder::limit(1, Sell, 100, 5).account(7));
    books.new_order(NewOrder::limit(2, Sell, 100, 5).account(8));
    // Cancel-oldest skips own order 1, so only 5 lots are really available.
    let skip = NewOrder::limit(3, Buy, 100, 6)
        .account(7)
        .stp(StpMode::CancelOldest)
        .tif(TimeInForce::Fok);
    assert_eq!(
        books.new_order(skip),
        [rejected(3, RejectReason::FillOrKillUnfillable)]
    );
    assert_eq!(
        books.fast.len(),
        2,
        "rejected FOK must not trigger self-trade cancels"
    );
    let fits = NewOrder::limit(4, Buy, 100, 5)
        .account(7)
        .stp(StpMode::CancelOldest)
        .tif(TimeInForce::Fok);
    assert_eq!(
        books.new_order(fits),
        [
            accepted(4, Buy, Some(100), 5),
            canceled(1, 5, CancelReason::SelfTrade),
            trade(4, 2, Buy, 100, 5, 0, 0)
        ]
    );
}

#[test]
fn depth_aggregates_levels_best_first() {
    let mut books = Books::new();
    books.limit(1, Buy, 99, 5);
    books.limit(2, Buy, 98, 1);
    books.limit(3, Buy, 99, 2);
    books.limit(4, Sell, 101, 3);
    books.limit(5, Sell, 103, 4);
    let depth = books.fast.depth(5);
    assert_eq!(depth, books.reference.depth(5));
    let bids: Vec<_> = depth
        .bids
        .iter()
        .map(|l| (l.price, l.qty, l.orders))
        .collect();
    let asks: Vec<_> = depth
        .asks
        .iter()
        .map(|l| (l.price, l.qty, l.orders))
        .collect();
    assert_eq!(bids, [(99, 7, 2), (98, 1, 1)]);
    assert_eq!(asks, [(101, 3, 1), (103, 4, 1)]);
    assert_eq!(books.fast.spread(), Some(2));
    assert_eq!(books.fast.depth(1).bids.len(), 1);
    assert_eq!(books.fast.level_count(Buy), 2);
    assert_eq!(books.fast.level_count(Sell), 2);
}

#[test]
fn slots_are_reused_after_orders_leave() {
    let mut books = Books::new();
    for round in 0..50u64 {
        books.limit(round * 2 + 1, Sell, 100, 5);
        books.limit(round * 2 + 2, Buy, 100, 5);
    }
    assert!(books.fast.is_empty());
    assert_eq!(books.fast.state_hash(), Engine::new().state_hash());
}

// ---------------------------------------------------------------------------------------
// Call auctions

fn uncrossed(price: i64, volume: u64, side: Option<Side>, imbalance: u64) -> Event {
    Event::Uncrossed {
        price: Some(price),
        volume,
        imbalance_side: side,
        imbalance,
    }
}

fn auction_trade(
    buy: u64,
    sell: u64,
    price: i64,
    qty: u64,
    buy_left: u64,
    sell_left: u64,
) -> Event {
    Event::AuctionTrade {
        buy,
        sell,
        price,
        qty,
        buy_remaining: buy_left,
        sell_remaining: sell_left,
    }
}

const NOTHING: Event = Event::Uncrossed {
    price: None,
    volume: 0,
    imbalance_side: None,
    imbalance: 0,
};

#[test]
fn a_call_phase_collects_orders_without_matching() {
    let mut books = Books::new();
    books.limit(1, Sell, 101, 5);
    assert_eq!(
        books.start_call(Some(100)),
        [Event::CallStarted {
            reference: Some(100)
        }]
    );
    assert_eq!(books.fast.phase(), Phase::Call);
    // A buy through the ask rests instead of trading, and the book crosses.
    assert_eq!(
        books.limit(2, Buy, 102, 3),
        [accepted(2, Buy, Some(102), 3), booked(2, Buy, 102, 3)]
    );
    assert_eq!(books.fast.spread(), Some(-1));
    // A GTC market order waits for the uncross, outside every price level.
    assert_eq!(
        books.market(3, Buy, 4),
        [
            accepted(3, Buy, None, 4),
            Event::MarketBooked {
                id: 3,
                side: Buy,
                qty: 4
            }
        ]
    );
    assert_eq!(books.fast.market_orders(Buy)[0].id, 3);
    assert!(books.fast.order(3).is_some_and(|order| order.is_market()));
    assert_eq!(books.fast.depth(5).bids.len(), 1);
    // 5 lots can trade at 101 or 102, each leaving 2 to buy: buyers in excess, so 102.
    assert_eq!(
        books.fast.indicative_uncross(),
        UncrossSummary {
            price: Some(102),
            volume: 5,
            imbalance_side: Some(Buy),
            imbalance: 2,
        }
    );
    // The market order fills first. The sell at 101 gets the clearing price, 102.
    assert_eq!(
        books.run(Command::Uncross),
        [
            auction_trade(3, 1, 102, 4, 0, 1),
            auction_trade(2, 1, 102, 1, 2, 0),
            uncrossed(102, 5, Some(Buy), 2),
        ]
    );
    assert_eq!(
        books.fast.phase(),
        Phase::Call,
        "an uncross keeps the call phase"
    );
    assert_eq!(books.fast.last_trade_price(), Some(102));
    assert_eq!(books.run(Command::Resume), [Event::Resumed]);
    assert_eq!(books.fast.order(2).map(|order| order.remaining), Some(2));
    assert_eq!(books.fast.reference_price(), None);
}

#[test]
fn immediate_and_post_only_orders_need_continuous_trading() {
    let mut books = Books::new();
    books.new_order(NewOrder::limit(1, Buy, 99, 5).post_only());
    books.limit(2, Sell, 101, 5);
    // Auction trades take liquidity, so resting post-only orders leave when the call starts.
    assert_eq!(
        books.start_call(None),
        [
            Event::CallStarted { reference: None },
            canceled(1, 5, CancelReason::CallPhase)
        ]
    );
    let needs = |id| [rejected(id, RejectReason::NeedsContinuousTrading)];
    for order in [
        NewOrder::limit(3, Buy, 101, 1).tif(TimeInForce::Ioc),
        NewOrder::limit(3, Buy, 101, 1).tif(TimeInForce::Fok),
        NewOrder::market(3, Buy, 1),
        NewOrder::market(3, Buy, 1).tif(TimeInForce::Fok),
        NewOrder::limit(3, Buy, 99, 1).post_only(),
    ] {
        assert_eq!(books.new_order(order), needs(3), "{order:?}");
    }
    // Quantity and price are still checked first, and ids must still be unique.
    let ioc = |price, qty| NewOrder::limit(4, Buy, price, qty).tif(TimeInForce::Ioc);
    assert_eq!(
        books.new_order(ioc(0, 5)),
        [rejected(4, RejectReason::InvalidPrice)]
    );
    assert_eq!(
        books.new_order(ioc(100, 0)),
        [rejected(4, RejectReason::InvalidQuantity)]
    );
    assert_eq!(
        books.limit(2, Buy, 100, 1),
        [rejected(2, RejectReason::DuplicateOrderId)]
    );
    // Back in continuous trading, a GTC market order cannot rest again.
    books.run(Command::Resume);
    assert_eq!(
        books.new_order(NewOrder::market(5, Buy, 1).tif(TimeInForce::Gtc)),
        [rejected(5, RejectReason::MarketOrderCannotRest)]
    );
}

#[test]
fn rule_1_the_highest_executable_volume() {
    let mut books = Books::new();
    books.start_call(None);
    for (id, side, price) in [
        (1, Buy, 100),
        (2, Buy, 101),
        (3, Buy, 102),
        (4, Sell, 100),
        (5, Sell, 101),
    ] {
        books.limit(id, side, price, 4);
    }
    // 8 lots trade at 101, 4 at 100 or 102. The bid at 100 is worse than 101 and stays.
    assert_eq!(
        books.run(Command::Uncross),
        [
            auction_trade(3, 4, 101, 4, 0, 0),
            auction_trade(2, 5, 101, 4, 0, 0),
            uncrossed(101, 8, None, 0),
        ]
    );
    assert_eq!(books.fast.order(1).map(|order| order.remaining), Some(4));
}

#[test]
fn rule_2_the_lowest_surplus() {
    let mut books = Books::new();
    books.start_call(None);
    books.limit(1, Buy, 100, 3);
    books.limit(2, Buy, 102, 5);
    books.limit(3, Sell, 100, 5);
    books.limit(4, Sell, 102, 4);
    // 5 lots at 100 leave 3 to buy; 5 at 102 leave 4 to sell.
    let events = books.run(Command::Uncross);
    assert_eq!(events.last(), Some(&uncrossed(100, 5, Some(Buy), 3)));
}

#[test]
fn rule_3_market_pressure() {
    // A buy surplus at both 100 and 101: the higher price, though the reference is lower.
    let mut books = Books::new();
    books.start_call(Some(50));
    books.limit(1, Buy, 101, 8);
    books.limit(2, Sell, 100, 5);
    assert_eq!(
        books.run(Command::Uncross),
        [
            auction_trade(1, 2, 101, 5, 3, 0),
            uncrossed(101, 5, Some(Buy), 3)
        ]
    );
    // A sell surplus: the lower price, though the reference is higher.
    let mut books = Books::new();
    books.start_call(Some(500));
    books.limit(1, Buy, 101, 5);
    books.limit(2, Sell, 100, 8);
    assert_eq!(
        books.run(Command::Uncross),
        [
            auction_trade(1, 2, 100, 5, 0, 3),
            uncrossed(100, 5, Some(Sell), 3)
        ]
    );
}

#[test]
fn rule_4_the_reference_price() {
    // No surplus at 100 or 101: the price nearer the reference.
    for (reference, price) in [(90, 100), (101, 101), (200, 101)] {
        let mut books = Books::new();
        books.start_call(Some(reference));
        books.limit(1, Buy, 101, 5);
        books.limit(2, Sell, 100, 5);
        assert_eq!(
            books.run(Command::Uncross).last(),
            Some(&uncrossed(price, 5, None, 0)),
            "reference {reference}"
        );
    }
    // Without a reference given, the call uses the last trade, here 101.
    let mut books = Books::new();
    books.limit(1, Sell, 101, 1);
    books.limit(2, Buy, 101, 1);
    assert_eq!(
        books.start_call(None),
        [Event::CallStarted {
            reference: Some(101)
        }]
    );
    books.limit(3, Buy, 101, 5);
    books.limit(4, Sell, 100, 5);
    assert_eq!(
        books.run(Command::Uncross).last(),
        Some(&uncrossed(101, 5, None, 0))
    );
    // A reference of zero or below is ignored.
    assert_eq!(
        books.start_call(Some(0)),
        [Event::CallStarted {
            reference: Some(101)
        }]
    );
}

#[test]
fn rule_5_the_lowest_price() {
    // No reference price at all.
    let mut books = Books::new();
    books.start_call(None);
    books.limit(1, Buy, 101, 5);
    books.limit(2, Sell, 100, 5);
    assert_eq!(
        books.run(Command::Uncross).last(),
        Some(&uncrossed(100, 5, None, 0))
    );
    // Mixed surplus sides with the reference exactly halfway.
    let mut books = Books::new();
    books.start_call(Some(101));
    books.limit(1, Buy, 100, 3);
    books.limit(2, Buy, 102, 5);
    books.limit(3, Sell, 100, 5);
    books.limit(4, Sell, 102, 3);
    assert_eq!(
        books.run(Command::Uncross).last(),
        Some(&uncrossed(100, 5, Some(Buy), 3))
    );
}

#[test]
fn market_orders_alone_clear_at_the_reference_price() {
    let mut books = Books::new();
    books.start_call(Some(99));
    books.market(1, Buy, 7);
    books.market(2, Sell, 4);
    assert_eq!(
        books.run(Command::Uncross),
        [
            auction_trade(1, 2, 99, 4, 3, 0),
            canceled(1, 3, CancelReason::Unfilled),
            uncrossed(99, 4, Some(Buy), 3),
        ]
    );
    assert!(books.fast.is_empty());
    // Without a reference price nothing can trade, and the market orders are canceled.
    let mut books = Books::new();
    books.start_call(None);
    books.market(1, Buy, 7);
    books.market(2, Sell, 4);
    assert_eq!(
        books.run(Command::Uncross),
        [
            canceled(1, 7, CancelReason::Unfilled),
            canceled(2, 4, CancelReason::Unfilled),
            NOTHING,
        ]
    );
}

#[test]
fn the_uncross_fills_in_price_time_priority() {
    let mut books = Books::new();
    books.start_call(None);
    books.limit(1, Buy, 100, 3); // same price as #2 but earlier
    books.limit(2, Buy, 100, 3);
    books.limit(3, Buy, 101, 2); // better price
    books.market(4, Buy, 1); // market orders come first
    books.limit(5, Sell, 100, 5);
    books.limit(6, Sell, 99, 2);
    books.limit(7, Sell, 102, 9); // beyond the clearing price: untouched
    let events = books.run(Command::Uncross);
    assert_eq!(
        events,
        [
            auction_trade(4, 6, 100, 1, 0, 1),
            auction_trade(3, 6, 100, 1, 1, 0),
            auction_trade(3, 5, 100, 1, 0, 4),
            auction_trade(1, 5, 100, 3, 0, 1),
            auction_trade(2, 5, 100, 1, 2, 0),
            uncrossed(100, 7, Some(Buy), 2),
        ]
    );
    assert_eq!(books.fast.order(2).map(|order| order.remaining), Some(2));
    assert_eq!(books.fast.order(7).map(|order| order.remaining), Some(9));
    assert_eq!(
        books.fast.spread(),
        Some(2),
        "the uncrossed book: bid 100, ask 102"
    );
}

#[test]
fn self_trade_prevention_does_not_apply_at_the_uncross() {
    let mut books = Books::new();
    books.start_call(None);
    books.new_order(
        NewOrder::limit(1, Buy, 101, 5)
            .account(7)
            .stp(StpMode::CancelBoth),
    );
    books.new_order(
        NewOrder::limit(2, Sell, 100, 5)
            .account(7)
            .stp(StpMode::CancelBoth),
    );
    assert_eq!(
        books.run(Command::Uncross),
        [
            auction_trade(1, 2, 100, 5, 0, 0),
            uncrossed(100, 5, None, 0)
        ]
    );
}

#[test]
fn modifies_in_a_call_phase_never_trade() {
    let mut books = Books::new();
    books.limit(1, Sell, 101, 5);
    books.limit(2, Buy, 99, 5);
    books.start_call(None);
    // Repricing through the ask re-enters the order without trading.
    assert_eq!(
        books.run(Command::Modify {
            id: 2,
            price: 102,
            qty: 5
        }),
        [
            Event::Modified {
                id: 2,
                price: 102,
                qty: 5,
                kept_priority: false
            },
            booked(2, Buy, 102, 5)
        ]
    );
    // A market order has no price to keep, so a modify turns it into a limit order.
    books.market(3, Buy, 4);
    assert_eq!(
        books.run(Command::Modify {
            id: 3,
            price: 100,
            qty: 4
        }),
        [
            Event::Modified {
                id: 3,
                price: 100,
                qty: 4,
                kept_priority: false
            },
            booked(3, Buy, 100, 4)
        ]
    );
    assert!(books.fast.market_orders(Buy).is_empty());
    assert_eq!(
        books.run(Command::Cancel { id: 3 }),
        [canceled(3, 4, CancelReason::Requested)]
    );
}

#[test]
fn resume_uncrosses_a_crossed_book_first() {
    let mut books = Books::new();
    books.start_call(Some(100));
    books.limit(1, Buy, 101, 5);
    books.limit(2, Sell, 100, 3);
    books.market(3, Sell, 1);
    // 4 lots at 100 or 101, each leaving 1 to buy: market pressure picks 101 before the
    // reference price is consulted.
    assert_eq!(
        books.run(Command::Resume),
        [
            auction_trade(1, 3, 101, 1, 4, 0),
            auction_trade(1, 2, 101, 3, 1, 0),
            uncrossed(101, 4, Some(Buy), 1),
            Event::Resumed,
        ]
    );
    assert_eq!(books.fast.phase(), Phase::Continuous);
    // A waiting market order alone is enough reason to uncross: it cannot rest.
    books.start_call(None);
    books.market(4, Buy, 2);
    assert_eq!(
        books.run(Command::Resume),
        [
            canceled(4, 2, CancelReason::Unfilled),
            NOTHING,
            Event::Resumed
        ]
    );
}

#[test]
fn phase_commands_are_valid_in_continuous_trading() {
    let mut books = Books::new();
    books.limit(1, Buy, 99, 5);
    books.limit(2, Sell, 101, 5);
    // The book never crosses in continuous trading, so an uncross finds nothing to do.
    assert_eq!(books.run(Command::Uncross), [NOTHING]);
    assert_eq!(books.run(Command::Resume), [Event::Resumed]);
    // Starting a call twice just replaces the reference price.
    books.start_call(Some(100));
    assert_eq!(
        books.start_call(Some(98)),
        [Event::CallStarted {
            reference: Some(98)
        }]
    );
    assert_eq!(books.fast.reference_price(), Some(98));
    assert_eq!(books.fast.len(), 2);
}
