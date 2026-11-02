//! A market-data feed built from the event stream alone.
//!
//! Exchanges publish trades and the total quantity at each price (market by price, or
//! L2). Both follow from the engine's events: [`Publisher`] keeps its own record of every
//! resting order from `Booked`, `Trade`, `Canceled` and `Modified` events and never looks
//! at the engine. [`BookBuilder`] is the subscriber. It applies the messages to its own
//! book and uses sequence numbers to notice a lost message, which it repairs from a
//! [`BookSnapshot`]. The tests check after every command that a subscriber's book equals
//! the engine's.
//!
//! For each command the publisher sends the trade prints, auction results and phase
//! changes first, in the order they happened, then one message per price level the command
//! changed, carrying the level's new totals. A total of zero removes the level. Level
//! messages are sorted bids first, best price first, so the feed is a deterministic
//! function of the commands.
//!
//! Through a call phase the levels may cross, and market orders wait outside every level,
//! so the feed shows only limit levels, as [`crate::Engine::depth`] does. An uncross is one
//! [`Message::Auction`] print with the clearing price, volume and surplus, followed by the
//! levels it changed; the individual auction fills are executions for their owners, not
//! public prints.

use core::fmt;
use std::collections::{BTreeMap, HashMap};

use crate::types::{Depth, Event, LevelView, OrderId, Phase, Price, Qty, Side, UncrossSummary};

/// One market-data message. Sequence numbers start at 1 and have no gaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    /// A trade.
    Trade {
        /// Sequence number.
        seq: u64,
        /// Execution price.
        price: Price,
        /// Executed quantity.
        qty: Qty,
        /// Side of the incoming order that took liquidity.
        aggressor: Side,
    },
    /// The totals at one price level after a command. `qty == 0` removes the level.
    Level {
        /// Sequence number.
        seq: u64,
        /// Side of the level.
        side: Side,
        /// Price of the level.
        price: Price,
        /// Total open quantity.
        qty: Qty,
        /// Number of resting orders.
        orders: u32,
    },
    /// The book entered a call phase or returned to continuous trading.
    Status {
        /// Sequence number.
        seq: u64,
        /// The new phase.
        phase: Phase,
        /// The auction's reference price in a call phase.
        reference: Option<Price>,
    },
    /// The result of an uncross: one print for every lot that changed hands at the clearing
    /// price. `price` is `None` when nothing could execute.
    Auction {
        /// Sequence number.
        seq: u64,
        /// Clearing price, volume and surplus.
        result: UncrossSummary,
    },
}

impl Message {
    /// The message's sequence number.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        match *self {
            Self::Trade { seq, .. }
            | Self::Level { seq, .. }
            | Self::Status { seq, .. }
            | Self::Auction { seq, .. } => seq,
        }
    }
}

/// Every price level at one point in the feed, for a subscriber that joins late or lost a
/// message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BookSnapshot {
    /// Sequence number of the last message the snapshot includes.
    pub seq: u64,
    /// All levels, best price first on each side.
    pub depth: Depth,
    /// The trading phase.
    pub phase: Phase,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Totals {
    qty: Qty,
    orders: u32,
}

/// Turns each command's events into market-data messages.
#[derive(Clone, Debug, Default)]
pub struct Publisher {
    resting: HashMap<OrderId, (Side, Price, Qty)>,
    /// Market orders waiting for an uncross; they belong to no level.
    market: HashMap<OrderId, (Side, Qty)>,
    bids: BTreeMap<Price, Totals>,
    asks: BTreeMap<Price, Totals>,
    touched: Vec<(Side, Price)>,
    seq: u64,
    phase: Phase,
}

impl Publisher {
    /// A publisher for a feed that starts with an empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sequence number of the last message sent, 0 before the first.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// Appends the messages for one command's `events` to `out`.
    ///
    /// # Panics
    /// Panics when the events contradict earlier ones, such as a trade against an order
    /// that is not resting. Events from [`crate::Engine`] never do.
    pub fn publish(&mut self, events: &[Event], out: &mut Vec<Message>) {
        self.touched.clear();
        for event in events {
            match *event {
                Event::Booked {
                    id,
                    side,
                    price,
                    qty,
                } => {
                    let previous = self.resting.insert(id, (side, price, qty));
                    assert!(previous.is_none(), "order {id} booked twice");
                    let level = self.book(side).entry(price).or_default();
                    level.qty += qty;
                    level.orders += 1;
                    self.touched.push((side, price));
                }
                Event::Trade {
                    maker,
                    taker_side,
                    price,
                    qty,
                    maker_remaining,
                    ..
                } => {
                    let side = taker_side.opposite();
                    let open = self.resting_order(maker, side, price);
                    assert_eq!(open - qty, maker_remaining, "order {maker}: trade quantity");
                    if maker_remaining == 0 {
                        self.resting.remove(&maker);
                        self.reduce(side, price, qty, 1);
                    } else {
                        self.resting.insert(maker, (side, price, maker_remaining));
                        self.reduce(side, price, qty, 0);
                    }
                    self.seq += 1;
                    out.push(Message::Trade {
                        seq: self.seq,
                        price,
                        qty,
                        aggressor: taker_side,
                    });
                }
                // A cancel of an order that is not resting removes the unfilled part of
                // an incoming order, which never reached the book.
                Event::Canceled { id, remaining, .. } => {
                    if let Some((side, price, open)) = self.resting.remove(&id) {
                        assert_eq!(open, remaining, "order {id}: canceled quantity");
                        self.reduce(side, price, open, 1);
                    } else if let Some((_, open)) = self.market.remove(&id) {
                        assert_eq!(open, remaining, "market order {id}: canceled quantity");
                    }
                }
                Event::Modified {
                    id,
                    price,
                    qty,
                    kept_priority,
                } => {
                    // A market order that gets a price leaves the market queue; its
                    // `Booked` event follows.
                    if self.market.remove(&id).is_some() {
                        assert!(!kept_priority, "market order {id}: in-place modify");
                        continue;
                    }
                    let &(side, old_price, open) = self
                        .resting
                        .get(&id)
                        .unwrap_or_else(|| panic!("order {id} modified but not resting"));
                    if kept_priority {
                        assert!(
                            old_price == price && qty <= open,
                            "order {id}: in-place modify"
                        );
                        self.resting.insert(id, (side, price, qty));
                        self.reduce(side, price, open - qty, 0);
                    } else {
                        // The order leaves the book; its trades and `Booked` follow.
                        self.resting.remove(&id);
                        self.reduce(side, old_price, open, 1);
                    }
                }
                Event::MarketBooked { id, side, qty } => {
                    let previous = self.market.insert(id, (side, qty));
                    assert!(previous.is_none(), "market order {id} booked twice");
                }
                Event::AuctionTrade {
                    buy,
                    sell,
                    price,
                    qty,
                    buy_remaining,
                    sell_remaining,
                } => {
                    self.auction_fill(buy, Side::Buy, price, qty, buy_remaining);
                    self.auction_fill(sell, Side::Sell, price, qty, sell_remaining);
                }
                Event::Uncrossed {
                    price,
                    volume,
                    imbalance_side,
                    imbalance,
                } => {
                    self.seq += 1;
                    out.push(Message::Auction {
                        seq: self.seq,
                        result: UncrossSummary {
                            price,
                            volume,
                            imbalance_side,
                            imbalance,
                        },
                    });
                }
                Event::CallStarted { reference } => self.status(Phase::Call, reference, out),
                Event::Resumed => self.status(Phase::Continuous, None, out),
                Event::Accepted { .. } | Event::Rejected { .. } => {}
            }
        }
        self.touched
            .sort_unstable_by_key(|&(side, price)| match side {
                Side::Buy => (0, -price),
                Side::Sell => (1, price),
            });
        self.touched.dedup();
        for index in 0..self.touched.len() {
            let (side, price) = self.touched[index];
            let totals = self.book(side).get(&price).copied().unwrap_or_default();
            self.seq += 1;
            out.push(Message::Level {
                seq: self.seq,
                side,
                price,
                qty: totals.qty,
                orders: totals.orders,
            });
        }
    }

    /// Every level as of the last message sent.
    #[must_use]
    pub fn snapshot(&self) -> BookSnapshot {
        BookSnapshot {
            seq: self.seq,
            depth: depth_of(&self.bids, &self.asks, usize::MAX),
            phase: self.phase,
        }
    }

    fn status(&mut self, phase: Phase, reference: Option<Price>, out: &mut Vec<Message>) {
        self.phase = phase;
        self.seq += 1;
        out.push(Message::Status {
            seq: self.seq,
            phase,
            reference,
        });
    }

    /// One order's side of an auction trade. A limit order trades at the clearing price,
    /// which its limit must allow, and leaves its own level; a market order has no level.
    fn auction_fill(&mut self, id: OrderId, side: Side, price: Price, qty: Qty, remaining: Qty) {
        if let Some(&(order_side, limit, open)) = self.resting.get(&id) {
            assert!(
                order_side == side && crate::types::crosses(side.opposite(), Some(price), limit),
                "auction trade at {price} against order {id}, which rests as {side} {limit}"
            );
            assert_eq!(open - qty, remaining, "order {id}: auction trade quantity");
            if remaining == 0 {
                self.resting.remove(&id);
                self.reduce(side, limit, qty, 1);
            } else {
                self.resting.insert(id, (side, limit, remaining));
                self.reduce(side, limit, qty, 0);
            }
            return;
        }
        let Some(&(order_side, open)) = self.market.get(&id) else {
            panic!("auction trade for order {id}, which is not resting");
        };
        assert_eq!(order_side, side, "market order {id}: auction trade side");
        assert_eq!(
            open - qty,
            remaining,
            "market order {id}: auction trade quantity"
        );
        if remaining == 0 {
            self.market.remove(&id);
        } else {
            self.market.insert(id, (side, remaining));
        }
    }

    fn book(&mut self, side: Side) -> &mut BTreeMap<Price, Totals> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    fn resting_order(&self, id: OrderId, side: Side, price: Price) -> Qty {
        match self.resting.get(&id) {
            Some(&(s, p, open)) if s == side && p == price => open,
            other => panic!("trade against order {id} at {price}, which rests as {other:?}"),
        }
    }

    fn reduce(&mut self, side: Side, price: Price, qty: Qty, orders: u32) {
        let book = self.book(side);
        let level = book.get_mut(&price).expect("level of a resting order");
        level.qty -= qty;
        level.orders -= orders;
        if level.orders == 0 {
            assert_eq!(
                level.qty, 0,
                "{side} level {price}: quantity without orders"
            );
            book.remove(&price);
        }
        self.touched.push((side, price));
    }
}

/// A subscriber detected a lost message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    /// The sequence number the subscriber needed next.
    pub expected: u64,
    /// The sequence number that arrived or was announced instead.
    pub received: u64,
}

impl fmt::Display for Gap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "market data gap: expected message {}, got {}",
            self.expected, self.received
        )
    }
}

impl std::error::Error for Gap {}

/// A subscriber's book, rebuilt from market-data messages.
#[derive(Clone, Debug)]
pub struct BookBuilder {
    bids: BTreeMap<Price, Totals>,
    asks: BTreeMap<Price, Totals>,
    next: u64,
    gap: Option<Gap>,
    last_trade: Option<(Price, Qty, Side)>,
    last_auction: Option<UncrossSummary>,
    phase: Phase,
}

impl Default for BookBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BookBuilder {
    /// A subscriber that starts with the feed's first message and an empty book.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            next: 1,
            gap: None,
            last_trade: None,
            last_auction: None,
            phase: Phase::Continuous,
        }
    }

    /// A subscriber that starts from `snapshot` and applies the messages after it.
    #[must_use]
    pub fn from_snapshot(snapshot: &BookSnapshot) -> Self {
        let totals = |levels: &[LevelView]| {
            levels
                .iter()
                .map(|level| {
                    let orders = u32::try_from(level.orders).expect("level order count fits u32");
                    (
                        level.price,
                        Totals {
                            qty: level.qty,
                            orders,
                        },
                    )
                })
                .collect()
        };
        Self {
            bids: totals(&snapshot.depth.bids),
            asks: totals(&snapshot.depth.asks),
            next: snapshot.seq + 1,
            gap: None,
            last_trade: None,
            last_auction: None,
            phase: snapshot.phase,
        }
    }

    /// Applies one message. A message the book already includes is ignored, so a
    /// subscriber can resynchronize and then replay buffered messages safely.
    ///
    /// # Errors
    /// Returns [`Gap`] when a message is missing. The builder then refuses every message
    /// until [`Self::resync`].
    pub fn apply(&mut self, message: &Message) -> Result<(), Gap> {
        if let Some(gap) = self.gap {
            return Err(gap);
        }
        let seq = message.seq();
        if seq < self.next {
            return Ok(());
        }
        if seq > self.next {
            return Err(self.lost(seq));
        }
        match *message {
            Message::Trade {
                price,
                qty,
                aggressor,
                ..
            } => self.last_trade = Some((price, qty, aggressor)),
            Message::Level {
                side,
                price,
                qty,
                orders,
                ..
            } => {
                let book = match side {
                    Side::Buy => &mut self.bids,
                    Side::Sell => &mut self.asks,
                };
                if qty == 0 {
                    book.remove(&price);
                } else {
                    book.insert(price, Totals { qty, orders });
                }
            }
            Message::Status { phase, .. } => self.phase = phase,
            Message::Auction { result, .. } => self.last_auction = Some(result),
        }
        self.next += 1;
        Ok(())
    }

    /// Checks a heartbeat that announces the feed's latest sequence number. A heartbeat is
    /// how a subscriber notices that the last messages before a quiet period were lost.
    ///
    /// # Errors
    /// Returns [`Gap`] when the subscriber is behind the announced sequence number.
    pub fn heartbeat(&mut self, latest: u64) -> Result<(), Gap> {
        if let Some(gap) = self.gap {
            return Err(gap);
        }
        if latest >= self.next {
            return Err(self.lost(latest));
        }
        Ok(())
    }

    /// Replaces the book with `snapshot` and clears a detected gap.
    pub fn resync(&mut self, snapshot: &BookSnapshot) {
        let (last_trade, last_auction) = (self.last_trade, self.last_auction);
        *self = Self::from_snapshot(snapshot);
        self.last_trade = last_trade;
        self.last_auction = last_auction;
    }

    /// `false` after a detected gap, until [`Self::resync`].
    #[must_use]
    pub const fn is_synced(&self) -> bool {
        self.gap.is_none()
    }

    /// Sequence number of the next message the builder needs.
    #[must_use]
    pub const fn next_seq(&self) -> u64 {
        self.next
    }

    /// The most recent continuous trade: price, quantity and aggressor side.
    #[must_use]
    pub const fn last_trade(&self) -> Option<(Price, Qty, Side)> {
        self.last_trade
    }

    /// The result of the most recent uncross.
    #[must_use]
    pub const fn last_auction(&self) -> Option<UncrossSummary> {
        self.last_auction
    }

    /// The trading phase, as the feed last announced it.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Up to `levels` price levels per side, best first, as [`crate::Engine::depth`] reports them.
    #[must_use]
    pub fn depth(&self, levels: usize) -> Depth {
        depth_of(&self.bids, &self.asks, levels)
    }

    fn lost(&mut self, received: u64) -> Gap {
        let gap = Gap {
            expected: self.next,
            received,
        };
        self.gap = Some(gap);
        gap
    }
}

fn depth_of(
    bids: &BTreeMap<Price, Totals>,
    asks: &BTreeMap<Price, Totals>,
    levels: usize,
) -> Depth {
    let view = |(&price, totals): (&Price, &Totals)| LevelView {
        price,
        qty: totals.qty,
        orders: totals.orders as usize,
    };
    Depth {
        bids: bids.iter().rev().take(levels).map(view).collect(),
        asks: asks.iter().take(levels).map(view).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;
    use crate::types::{Command, NewOrder};

    fn run(commands: &[Command]) -> (Engine, Publisher, Vec<Vec<Message>>) {
        let (mut engine, mut publisher) = (Engine::new(), Publisher::new());
        let mut batches = Vec::new();
        for command in commands {
            let (mut events, mut messages) = (Vec::new(), Vec::new());
            engine.process(command, &mut events);
            publisher.publish(&events, &mut messages);
            batches.push(messages);
        }
        (engine, publisher, batches)
    }

    #[test]
    fn a_sweep_prints_trades_then_updated_levels() {
        let (engine, _, batches) = run(&[
            Command::New(NewOrder::limit(1, Side::Sell, 101, 5)),
            Command::New(NewOrder::limit(2, Side::Sell, 102, 5)),
            Command::New(NewOrder::limit(3, Side::Buy, 102, 7)),
        ]);
        let mut builder = BookBuilder::new();
        assert_eq!(
            (
                builder.is_synced(),
                builder.next_seq(),
                builder.last_trade()
            ),
            (true, 1, None)
        );
        for message in batches.concat() {
            builder.apply(&message).unwrap();
        }
        assert_eq!(builder.next_seq(), 7);
        assert!(builder.is_synced());
        assert_eq!(builder.last_trade(), Some((102, 2, Side::Buy)));
        assert_eq!(builder.depth(usize::MAX), engine.depth(usize::MAX));
        assert_eq!(
            batches[2],
            [
                Message::Trade {
                    seq: 3,
                    price: 101,
                    qty: 5,
                    aggressor: Side::Buy
                },
                Message::Trade {
                    seq: 4,
                    price: 102,
                    qty: 2,
                    aggressor: Side::Buy
                },
                Message::Level {
                    seq: 5,
                    side: Side::Sell,
                    price: 101,
                    qty: 0,
                    orders: 0
                },
                Message::Level {
                    seq: 6,
                    side: Side::Sell,
                    price: 102,
                    qty: 3,
                    orders: 1
                },
            ]
        );
    }

    #[test]
    fn rejected_and_unfilled_orders_publish_nothing() {
        let (_, publisher, batches) = run(&[
            Command::Cancel { id: 9 },
            Command::New(NewOrder::market(1, Side::Buy, 5)),
        ]);
        assert!(batches.iter().all(Vec::is_empty));
        assert_eq!(publisher.seq(), 0);
    }

    fn trade(maker: OrderId, taker_side: Side, price: Price) -> Event {
        Event::Trade {
            taker: 99,
            maker,
            taker_side,
            price,
            qty: 1,
            taker_remaining: 0,
            maker_remaining: 4,
        }
    }

    #[test]
    #[should_panic(expected = "which rests as")]
    fn a_trade_against_an_order_on_the_same_side_is_refused() {
        let mut publisher = Publisher::new();
        let mut out = Vec::new();
        let booked = Event::Booked {
            id: 1,
            side: Side::Buy,
            price: 100,
            qty: 5,
        };
        publisher.publish(&[booked], &mut out);
        // An incoming buy can only trade against a resting sell.
        publisher.publish(&[trade(1, Side::Buy, 100)], &mut out);
    }

    #[test]
    #[should_panic(expected = "which rests as")]
    fn a_trade_at_another_price_than_the_resting_order_is_refused() {
        let mut publisher = Publisher::new();
        let mut out = Vec::new();
        let booked = Event::Booked {
            id: 1,
            side: Side::Sell,
            price: 100,
            qty: 5,
        };
        publisher.publish(&[booked], &mut out);
        publisher.publish(&[trade(1, Side::Buy, 101)], &mut out);
    }

    #[test]
    fn gaps_are_detected_and_repaired_from_a_snapshot() {
        let (engine, publisher, batches) = run(&[
            Command::New(NewOrder::limit(1, Side::Buy, 99, 5)),
            Command::New(NewOrder::limit(2, Side::Buy, 98, 5)),
            Command::Modify {
                id: 1,
                price: 99,
                qty: 2,
            },
        ]);
        let messages: Vec<Message> = batches.concat();
        let mut builder = BookBuilder::new();
        builder.apply(&messages[0]).unwrap();
        let gap = Gap {
            expected: 2,
            received: 3,
        };
        assert_eq!(builder.apply(&messages[2]), Err(gap));
        assert!(!builder.is_synced());
        assert_eq!(
            builder.apply(&messages[1]),
            Err(gap),
            "refuses until resync"
        );
        builder.resync(&publisher.snapshot());
        assert!(builder.is_synced());
        assert_eq!(builder.next_seq(), publisher.seq() + 1);
        for message in &messages {
            builder.apply(message).unwrap(); // already included, so ignored
        }
        assert_eq!(builder.depth(usize::MAX), engine.depth(usize::MAX));
        assert_eq!(builder.heartbeat(publisher.seq()), Ok(()));
        assert_eq!(
            builder.heartbeat(publisher.seq() + 2),
            Err(Gap {
                expected: 4,
                received: 5
            })
        );
    }
}
