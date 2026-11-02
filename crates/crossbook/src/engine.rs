//! The order book and matching engine.
//!
//! Layout:
//! - one `BTreeMap<Price, Level>` per side, so the best price is the first (asks) or
//!   last (bids) key and price levels are visited in priority order;
//! - every resting order is a node in one slab (`Vec<Node>`) with a free list, linked
//!   into its level's FIFO queue by `u32` indices, so cancel is O(1) after the lookup;
//! - an `OrderId -> slot` hash map for cancel and modify.
//!
//! During an auction call phase the same structures hold the call book, which may cross,
//! plus one FIFO queue of market orders per side. Market nodes carry price 0, which no
//! limit order can have. The uncross ([`crate::auction`]) reads the cached level totals to
//! find the clearing price, then fills each side's orders in priority order.
//!
//! The engine is single threaded and deterministic: it never reads clocks, randomness or
//! hash-map iteration order, so the same commands always produce the same events.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};

use crate::auction;
use crate::digest::Fingerprint;
use crate::types::{
    AccountId, CancelReason, Command, Depth, Event, LevelView, MARKET, MAX_QTY, NewOrder, OrderId,
    OrderView, Phase, Price, Qty, RejectReason, Side, StpMode, TimeInForce, UncrossSummary,
    crosses, static_reject,
};

const NIL: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
struct Node {
    id: OrderId,
    account: AccountId,
    side: Side,
    price: Price,
    remaining: Qty,
    priority: u64,
    stp: StpMode,
    post_only: bool,
    prev: u32,
    next: u32,
}

#[derive(Clone, Copy, Debug)]
struct Level {
    head: u32,
    tail: u32,
    total: Qty,
    count: u32,
}

impl Level {
    const EMPTY: Self = Self {
        head: NIL,
        tail: NIL,
        total: 0,
        count: 0,
    };
}

impl Default for Level {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Multiplicative hasher for integer order ids (FxHash with a final fold).
///
/// Order ids are trusted input in this design. A gateway facing untrusted clients would
/// assign ids itself or switch to a keyed hasher; see `docs/DESIGN.md`.
#[derive(Clone, Copy, Default)]
struct IdHasher(u64);

impl IdHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for IdHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.add(u64::from(byte));
        }
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }
}

type IdMap = HashMap<OrderId, u32, BuildHasherDefault<IdHasher>>;

#[derive(Clone, Copy)]
struct Taker {
    id: OrderId,
    account: AccountId,
    side: Side,
    limit: Option<Price>,
    stp: StpMode,
}

/// A limit order book for one instrument with price-time priority matching.
///
/// ```
/// use crossbook::{Command, Engine, Event, NewOrder, Side};
///
/// let mut engine = Engine::new();
/// let mut events = Vec::new();
/// engine.process(&Command::New(NewOrder::limit(1, Side::Sell, 101, 5)), &mut events);
/// engine.process(&Command::New(NewOrder::limit(2, Side::Buy, 102, 3)), &mut events);
///
/// // The buy trades at the resting sell's price, 101, not at its own limit.
/// assert!(events.contains(&Event::Trade {
///     taker: 2,
///     maker: 1,
///     taker_side: Side::Buy,
///     price: 101,
///     qty: 3,
///     taker_remaining: 0,
///     maker_remaining: 2,
/// }));
/// assert_eq!(engine.best_ask().map(|level| level.qty), Some(2));
/// ```
#[derive(Clone, Debug, Default)]
pub struct Engine {
    bids: BTreeMap<Price, Level>,
    asks: BTreeMap<Price, Level>,
    /// Market orders waiting for an uncross, in time priority. Empty in continuous trading.
    market_bids: Level,
    market_asks: Level,
    nodes: Vec<Node>,
    free: Vec<u32>,
    index: IdMap,
    clock: u64,
    phase: Phase,
    /// Reference price of the current call phase.
    reference: Option<Price>,
    last_trade: Option<Price>,
}

/// Engine state besides the resting orders, as a snapshot stores it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionState {
    pub clock: u64,
    pub phase: Phase,
    pub reference: Option<Price>,
    pub last_trade: Option<Price>,
}

impl Engine {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty book with room for `orders` resting orders before reallocating.
    #[must_use]
    pub fn with_capacity(orders: usize) -> Self {
        Self {
            nodes: Vec::with_capacity(orders),
            index: IdMap::with_capacity_and_hasher(orders, BuildHasherDefault::default()),
            ..Self::default()
        }
    }

    /// Applies one command and appends its events to `out`.
    ///
    /// `out` is not cleared, so one buffer can collect a whole session.
    pub fn process(&mut self, command: &Command, out: &mut Vec<Event>) {
        match *command {
            Command::New(ref order) => self.submit(order, out),
            Command::Cancel { id } => self.cancel(id, out),
            Command::Modify { id, price, qty } => self.modify(id, price, qty, out),
            Command::StartCall { reference } => self.start_call(reference, out),
            Command::Uncross => self.uncross(out),
            Command::Resume => self.resume(out),
        }
    }

    fn submit(&mut self, order: &NewOrder, out: &mut Vec<Event>) {
        // In a call phase `static_reject` has already turned away post-only, IOC and FOK
        // orders, so the book-state checks below cannot fire there.
        let reject = static_reject(order, self.phase)
            .or_else(|| {
                self.index
                    .contains_key(&order.id)
                    .then_some(RejectReason::DuplicateOrderId)
            })
            .or_else(|| {
                (order.post_only && self.would_cross(order.side, order.kind.limit()))
                    .then_some(RejectReason::PostOnlyWouldCross)
            })
            .or_else(|| {
                (order.tif == TimeInForce::Fok && !self.can_fill(order))
                    .then_some(RejectReason::FillOrKillUnfillable)
            });
        if let Some(reason) = reject {
            out.push(Event::Rejected {
                id: order.id,
                reason,
            });
            return;
        }

        out.push(Event::Accepted {
            id: order.id,
            side: order.side,
            price: order.kind.limit(),
            qty: order.qty,
        });
        if self.phase == Phase::Call {
            // Nothing trades on arrival in a call phase; a market order waits unpriced.
            self.rest(
                Node {
                    id: order.id,
                    account: order.account,
                    side: order.side,
                    price: order.kind.limit().unwrap_or(MARKET),
                    remaining: order.qty,
                    priority: 0,
                    stp: order.stp,
                    post_only: false,
                    prev: NIL,
                    next: NIL,
                },
                out,
            );
            return;
        }
        let taker = Taker {
            id: order.id,
            account: order.account,
            side: order.side,
            limit: order.kind.limit(),
            stp: order.stp,
        };
        let mut remaining = order.qty;
        if self.match_taker(taker, &mut remaining, out) || remaining == 0 {
            return;
        }
        debug_assert_ne!(
            order.tif,
            TimeInForce::Fok,
            "a checked FOK order filled partially"
        );
        match (order.kind.limit(), order.tif) {
            (Some(price), TimeInForce::Gtc) => self.rest(
                Node {
                    id: order.id,
                    account: order.account,
                    side: order.side,
                    price,
                    remaining,
                    priority: 0,
                    stp: order.stp,
                    post_only: order.post_only,
                    prev: NIL,
                    next: NIL,
                },
                out,
            ),
            _ => out.push(Event::Canceled {
                id: order.id,
                remaining,
                reason: CancelReason::Unfilled,
            }),
        }
    }

    fn cancel(&mut self, id: OrderId, out: &mut Vec<Event>) {
        let Some(slot) = self.index.remove(&id) else {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::UnknownOrder,
            });
            return;
        };
        let remaining = self.nodes[slot as usize].remaining;
        self.detach(slot);
        out.push(Event::Canceled {
            id,
            remaining,
            reason: CancelReason::Requested,
        });
    }

    fn modify(&mut self, id: OrderId, price: Price, qty: Qty, out: &mut Vec<Event>) {
        let Some(&slot) = self.index.get(&id) else {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::UnknownOrder,
            });
            return;
        };
        if qty == 0 || qty > MAX_QTY {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::InvalidQuantity,
            });
            return;
        }
        if price <= 0 {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::InvalidPrice,
            });
            return;
        }
        let node = self.nodes[slot as usize];

        // A market order waiting for an uncross has price 0, so it always takes the
        // re-entry path below and becomes a limit order.
        if price == node.price && qty <= node.remaining {
            let level = side_book(&mut self.bids, &mut self.asks, node.side)
                .get_mut(&price)
                .expect("resting order has a level");
            level.total -= node.remaining - qty;
            self.nodes[slot as usize].remaining = qty;
            out.push(Event::Modified {
                id,
                price,
                qty,
                kept_priority: true,
            });
            return;
        }

        if node.post_only && self.would_cross(node.side, Some(price)) {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::PostOnlyWouldCross,
            });
            return;
        }

        self.index.remove(&id);
        self.detach(slot);
        out.push(Event::Modified {
            id,
            price,
            qty,
            kept_priority: false,
        });
        if self.phase == Phase::Call {
            self.rest(
                Node {
                    price,
                    remaining: qty,
                    ..node
                },
                out,
            );
            return;
        }
        let taker = Taker {
            id,
            account: node.account,
            side: node.side,
            limit: Some(price),
            stp: node.stp,
        };
        let mut remaining = qty;
        if self.match_taker(taker, &mut remaining, out) || remaining == 0 {
            return;
        }
        // `rest` assigns fresh links and a new time priority.
        self.rest(
            Node {
                price,
                remaining,
                ..node
            },
            out,
        );
    }

    /// Matches an incoming order against the opposite side, best price first and oldest
    /// order first within a price. Returns `true` when self-trade prevention canceled it.
    fn match_taker(&mut self, taker: Taker, remaining: &mut Qty, out: &mut Vec<Event>) -> bool {
        let book = side_book(&mut self.bids, &mut self.asks, taker.side.opposite());
        while *remaining > 0 {
            let entry = match taker.side {
                Side::Buy => book.first_entry(),
                Side::Sell => book.last_entry(),
            };
            let Some(mut entry) = entry else { break };
            let price = *entry.key();
            if !crosses(taker.side, taker.limit, price) {
                break;
            }
            let level = entry.get_mut();
            let slot = level.head;
            let maker = self.nodes[slot as usize];

            if taker.stp != StpMode::Allow && maker.account == taker.account {
                if taker.stp != StpMode::CancelNewest {
                    unlink(level, &mut self.nodes, slot);
                    if level.count == 0 {
                        entry.remove();
                    }
                    self.index.remove(&maker.id);
                    self.free.push(slot);
                    out.push(Event::Canceled {
                        id: maker.id,
                        remaining: maker.remaining,
                        reason: CancelReason::SelfTrade,
                    });
                }
                if taker.stp != StpMode::CancelOldest {
                    out.push(Event::Canceled {
                        id: taker.id,
                        remaining: *remaining,
                        reason: CancelReason::SelfTrade,
                    });
                    *remaining = 0;
                    return true;
                }
                continue;
            }

            let qty = (*remaining).min(maker.remaining);
            let maker_remaining = maker.remaining - qty;
            *remaining -= qty;
            level.total -= qty;
            self.nodes[slot as usize].remaining = maker_remaining;
            self.last_trade = Some(price);
            out.push(Event::Trade {
                taker: taker.id,
                maker: maker.id,
                taker_side: taker.side,
                price,
                qty,
                taker_remaining: *remaining,
                maker_remaining,
            });
            if maker_remaining == 0 {
                unlink(level, &mut self.nodes, slot);
                if level.count == 0 {
                    entry.remove();
                }
                self.index.remove(&maker.id);
                self.free.push(slot);
            }
        }
        false
    }

    /// Adds `node` to the back of its price level, or of its side's market queue, with a
    /// fresh time priority.
    fn rest(&mut self, mut node: Node, out: &mut Vec<Event>) {
        self.clock += 1;
        node.priority = self.clock;
        let (id, side, price, qty) = (node.id, node.side, node.price, node.remaining);
        let slot = match self.free.pop() {
            Some(slot) => {
                self.nodes[slot as usize] = node;
                slot
            }
            None => {
                let slot = u32::try_from(self.nodes.len())
                    .ok()
                    .filter(|&slot| slot != NIL)
                    .expect("order book capacity exceeded");
                self.nodes.push(node);
                slot
            }
        };
        self.index.insert(id, slot);
        if price == MARKET {
            let queue = market_queue(&mut self.market_bids, &mut self.market_asks, side);
            link_back(queue, &mut self.nodes, slot);
            out.push(Event::MarketBooked { id, side, qty });
            return;
        }
        let level = side_book(&mut self.bids, &mut self.asks, side)
            .entry(price)
            .or_insert(Level::EMPTY);
        link_back(level, &mut self.nodes, slot);
        out.push(Event::Booked {
            id,
            side,
            price,
            qty,
        });
    }

    /// Removes a resting order from its level and frees its slot. The caller updates the index.
    fn detach(&mut self, slot: u32) {
        let Node { side, price, .. } = self.nodes[slot as usize];
        self.free.push(slot);
        if price == MARKET {
            let queue = market_queue(&mut self.market_bids, &mut self.market_asks, side);
            unlink(queue, &mut self.nodes, slot);
            return;
        }
        let Entry::Occupied(mut entry) =
            side_book(&mut self.bids, &mut self.asks, side).entry(price)
        else {
            unreachable!("resting order without a price level");
        };
        unlink(entry.get_mut(), &mut self.nodes, slot);
        if entry.get().count == 0 {
            entry.remove();
        }
    }

    /// Enters (or restarts) a call phase and cancels resting post-only orders in book order.
    fn start_call(&mut self, reference: Option<Price>, out: &mut Vec<Event>) {
        self.phase = Phase::Call;
        self.reference = reference.filter(|&price| price > 0).or(self.last_trade);
        out.push(Event::CallStarted {
            reference: self.reference,
        });
        let mut post_only = Vec::new();
        self.for_each_resting(|node| {
            if node.post_only {
                post_only.push(node.id);
            }
        });
        for id in post_only {
            let slot = self.index.remove(&id).expect("listed order rests");
            let remaining = self.nodes[slot as usize].remaining;
            self.detach(slot);
            out.push(Event::Canceled {
                id,
                remaining,
                reason: CancelReason::CallPhase,
            });
        }
    }

    /// Ends the call phase. A crossed book, or one with market orders waiting, is uncrossed
    /// first, so continuous trading never starts from a state it could not reach itself.
    fn resume(&mut self, out: &mut Vec<Event>) {
        let crossed = matches!(
            (self.bids.last_key_value(), self.asks.first_key_value()),
            (Some((bid, _)), Some((ask, _))) if bid >= ask
        );
        if crossed || self.market_bids.count + self.market_asks.count > 0 {
            self.uncross(out);
        }
        self.phase = Phase::Continuous;
        self.reference = None;
        out.push(Event::Resumed);
    }

    /// Executes the auction at one clearing price, then cancels what is left of the waiting
    /// market orders. Each side fills in priority order: market orders first, then better
    /// limits, then earlier orders. Each trade pairs the first unfilled buy with the first
    /// unfilled sell.
    fn uncross(&mut self, out: &mut Vec<Event>) {
        let summary = self.indicative_uncross();
        if let Some(price) = summary.price {
            let buys = self.allocate(Side::Buy, price, summary.volume);
            let sells = self.allocate(Side::Sell, price, summary.volume);
            let open = |slot: u32| self.nodes[slot as usize].remaining;
            let (mut buy, mut sell) = (0, 0);
            let (mut buy_left, mut sell_left) = (buys[0].1, sells[0].1);
            let (mut buy_open, mut sell_open) = (open(buys[0].0), open(sells[0].0));
            loop {
                let qty = buy_left.min(sell_left);
                buy_left -= qty;
                sell_left -= qty;
                buy_open -= qty;
                sell_open -= qty;
                out.push(Event::AuctionTrade {
                    buy: self.nodes[buys[buy].0 as usize].id,
                    sell: self.nodes[sells[sell].0 as usize].id,
                    price,
                    qty,
                    buy_remaining: buy_open,
                    sell_remaining: sell_open,
                });
                if buy_left == 0 {
                    buy += 1;
                    let Some(&(slot, fill)) = buys.get(buy) else {
                        break;
                    };
                    (buy_left, buy_open) = (fill, open(slot));
                }
                if sell_left == 0 {
                    sell += 1;
                    let Some(&(slot, fill)) = sells.get(sell) else {
                        break;
                    };
                    (sell_left, sell_open) = (fill, open(slot));
                }
            }
            for &(slot, fill) in buys.iter().chain(&sells) {
                self.fill(slot, fill);
            }
            self.last_trade = Some(price);
        }
        for side in [Side::Buy, Side::Sell] {
            let queue = std::mem::take(market_queue(
                &mut self.market_bids,
                &mut self.market_asks,
                side,
            ));
            let mut slot = queue.head;
            while slot != NIL {
                let node = self.nodes[slot as usize];
                self.index.remove(&node.id);
                self.free.push(slot);
                out.push(Event::Canceled {
                    id: node.id,
                    remaining: node.remaining,
                    reason: CancelReason::Unfilled,
                });
                slot = node.next;
            }
        }
        out.push(summary.event());
    }

    /// The orders on `side` that trade `volume` lots at `price`, with their fills, in
    /// priority order: the market queue, then the levels from the best price to `price`.
    /// Only the last one can be a partial fill.
    fn allocate(&self, side: Side, price: Price, volume: Qty) -> Vec<(u32, Qty)> {
        let once = std::iter::once;
        match side {
            Side::Buy => {
                let levels = self.bids.range(price..).rev().map(|(_, level)| level);
                self.collect_fills(once(&self.market_bids).chain(levels), volume)
            }
            Side::Sell => {
                let levels = self.asks.range(..=price).map(|(_, level)| level);
                self.collect_fills(once(&self.market_asks).chain(levels), volume)
            }
        }
    }

    /// Walks `queues` in order and takes orders until `volume` lots are covered.
    fn collect_fills<'a>(
        &self,
        queues: impl Iterator<Item = &'a Level>,
        volume: Qty,
    ) -> Vec<(u32, Qty)> {
        let mut fills = Vec::new();
        let mut left = volume;
        for level in queues {
            let mut slot = level.head;
            while slot != NIL {
                let node = &self.nodes[slot as usize];
                let fill = node.remaining.min(left);
                fills.push((slot, fill));
                left -= fill;
                if left == 0 {
                    return fills;
                }
                slot = node.next;
            }
        }
        fills
    }

    /// Takes `fill` lots from a resting order, removing it when it is used up.
    fn fill(&mut self, slot: u32, fill: Qty) {
        let node = self.nodes[slot as usize];
        if fill == node.remaining {
            self.index.remove(&node.id);
            self.detach(slot);
            return;
        }
        self.nodes[slot as usize].remaining -= fill;
        let level = if node.price == MARKET {
            market_queue(&mut self.market_bids, &mut self.market_asks, node.side)
        } else {
            side_book(&mut self.bids, &mut self.asks, node.side)
                .get_mut(&node.price)
                .expect("resting order has a level")
        };
        level.total -= fill;
    }

    /// What an uncross would execute now: the clearing price, volume and surplus that an
    /// exchange publishes during a call phase. In continuous trading the book never crosses,
    /// so nothing would execute.
    #[must_use]
    pub fn indicative_uncross(&self) -> UncrossSummary {
        auction::determine(
            self.bids.iter().map(|(&price, level)| (price, level.total)),
            self.asks.iter().map(|(&price, level)| (price, level.total)),
            self.market_bids.total,
            self.market_asks.total,
            self.reference,
        )
    }

    /// The current trading phase.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// The reference price of the current call phase: the one given to
    /// [`Command::StartCall`], or the last trade price when the call started. `None` in
    /// continuous trading.
    #[must_use]
    pub const fn reference_price(&self) -> Option<Price> {
        self.reference
    }

    /// The price of the most recent trade, continuous or auction.
    #[must_use]
    pub const fn last_trade_price(&self) -> Option<Price> {
        self.last_trade
    }

    /// Market orders waiting for an uncross on one side, in time priority. Their price is 0.
    #[must_use]
    pub fn market_orders(&self, side: Side) -> Vec<OrderView> {
        let queue = match side {
            Side::Buy => &self.market_bids,
            Side::Sell => &self.market_asks,
        };
        let mut views = Vec::new();
        self.walk(queue, |node| views.push(order_view(node)));
        views
    }

    /// True when an order on `side` at `limit` would trade immediately.
    fn would_cross(&self, side: Side, limit: Option<Price>) -> bool {
        let best = match side {
            Side::Buy => self.asks.first_key_value(),
            Side::Sell => self.bids.last_key_value(),
        };
        best.is_some_and(|(&price, _)| crosses(side, limit, price))
    }

    /// Fill-or-kill check: true when `order` would fill in full right now.
    ///
    /// This walks the same queue the matcher would, without changing anything. Resting
    /// orders that self-trade prevention would cancel do not count as liquidity, and a
    /// mode that cancels the incoming order ends the walk.
    fn can_fill(&self, order: &NewOrder) -> bool {
        let limit = order.kind.limit();
        let mut available: Qty = 0;
        let mut visit = |price: Price, level: &Level| -> Option<bool> {
            if !crosses(order.side, limit, price) {
                return Some(false);
            }
            let mut slot = level.head;
            while slot != NIL {
                let node = &self.nodes[slot as usize];
                if order.stp != StpMode::Allow && node.account == order.account {
                    if order.stp != StpMode::CancelOldest {
                        return Some(false);
                    }
                } else {
                    available += node.remaining;
                    if available >= order.qty {
                        return Some(true);
                    }
                }
                slot = node.next;
            }
            None
        };
        let outcome = match order.side {
            Side::Buy => self
                .asks
                .iter()
                .find_map(|(&price, level)| visit(price, level)),
            Side::Sell => self
                .bids
                .iter()
                .rev()
                .find_map(|(&price, level)| visit(price, level)),
        };
        outcome.unwrap_or(false)
    }

    /// Number of resting orders.
    #[must_use]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// True when no orders rest on either side.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Number of distinct price levels on one side.
    #[must_use]
    pub fn level_count(&self, side: Side) -> usize {
        match side {
            Side::Buy => self.bids.len(),
            Side::Sell => self.asks.len(),
        }
    }

    /// Highest bid level.
    #[must_use]
    pub fn best_bid(&self) -> Option<LevelView> {
        self.bids
            .last_key_value()
            .map(|(&price, level)| level_view(price, level))
    }

    /// Lowest ask level.
    #[must_use]
    pub fn best_ask(&self) -> Option<LevelView> {
        self.asks
            .first_key_value()
            .map(|(&price, level)| level_view(price, level))
    }

    /// Best ask minus best bid, when both sides have orders.
    #[must_use]
    pub fn spread(&self) -> Option<Price> {
        Some(self.best_ask()?.price - self.best_bid()?.price)
    }

    /// Up to `levels` price levels per side, best first.
    #[must_use]
    pub fn depth(&self, levels: usize) -> Depth {
        Depth {
            bids: self
                .bids
                .iter()
                .rev()
                .take(levels)
                .map(|(&p, l)| level_view(p, l))
                .collect(),
            asks: self
                .asks
                .iter()
                .take(levels)
                .map(|(&p, l)| level_view(p, l))
                .collect(),
        }
    }

    /// A resting order by id.
    #[must_use]
    pub fn order(&self, id: OrderId) -> Option<OrderView> {
        self.index
            .get(&id)
            .map(|&slot| order_view(&self.nodes[slot as usize]))
    }

    /// Orders resting at one price, in time priority (first to trade first).
    #[must_use]
    pub fn queue(&self, side: Side, price: Price) -> Vec<OrderView> {
        let book = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        let mut views = Vec::new();
        if let Some(level) = book.get(&price) {
            self.walk(level, |node| views.push(order_view(node)));
        }
        views
    }

    /// Every resting order in canonical order: bids from the best price down, then asks
    /// from the best price up, each level in time priority. In a call phase, each side's
    /// waiting market orders come first, as they have the highest priority in an uncross.
    #[must_use]
    pub fn snapshot(&self) -> Vec<OrderView> {
        let mut views = Vec::with_capacity(self.len());
        self.for_each_resting(|node| views.push(order_view(node)));
        views
    }

    /// Fingerprint of the book (see [`crate::digest::book`]). Equal books give equal hashes.
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        let mut fingerprint = Fingerprint::new();
        self.for_each_resting(|node| fingerprint.order(&order_view(node)));
        fingerprint.value()
    }

    /// Everything besides the resting orders that a snapshot must keep.
    pub(crate) const fn session_state(&self) -> SessionState {
        SessionState {
            clock: self.clock,
            phase: self.phase,
            reference: self.reference,
            last_trade: self.last_trade,
        }
    }

    /// Visits every resting order in [`Self::snapshot`] order with its time priority.
    pub(crate) fn for_each_with_priority(&self, mut visit: impl FnMut(OrderView, u64)) {
        self.for_each_resting(|node| visit(order_view(node), node.priority));
    }

    /// Rebuilds a book from resting orders listed in [`Self::snapshot`] order with their
    /// time priorities. The caller has validated them (see [`crate::snapshot::decode`]).
    pub(crate) fn restore(state: SessionState, orders: &[(OrderView, u64)]) -> Self {
        let mut engine = Self::with_capacity(orders.len());
        for &(view, priority) in orders {
            let slot = u32::try_from(engine.nodes.len())
                .ok()
                .filter(|&slot| slot != NIL)
                .expect("order book capacity exceeded");
            engine.nodes.push(Node {
                id: view.id,
                account: view.account,
                side: view.side,
                price: view.price,
                remaining: view.remaining,
                priority,
                stp: view.stp,
                post_only: view.post_only,
                prev: NIL,
                next: NIL,
            });
            let level = if view.price == MARKET {
                market_queue(&mut engine.market_bids, &mut engine.market_asks, view.side)
            } else {
                side_book(&mut engine.bids, &mut engine.asks, view.side)
                    .entry(view.price)
                    .or_insert(Level::EMPTY)
            };
            link_back(level, &mut engine.nodes, slot);
            engine.index.insert(view.id, slot);
        }
        engine.clock = state.clock;
        engine.phase = state.phase;
        engine.reference = state.reference;
        engine.last_trade = state.last_trade;
        engine
    }

    /// Visits resting orders in priority order: waiting market buys, bids from the best
    /// price down, waiting market sells, then asks from the best price up.
    fn for_each_resting(&self, mut visit: impl FnMut(&Node)) {
        self.walk(&self.market_bids, &mut visit);
        for level in self.bids.values().rev() {
            self.walk(level, &mut visit);
        }
        self.walk(&self.market_asks, &mut visit);
        for level in self.asks.values() {
            self.walk(level, &mut visit);
        }
    }

    fn walk(&self, level: &Level, mut visit: impl FnMut(&Node)) {
        let mut slot = level.head;
        while slot != NIL {
            let node = &self.nodes[slot as usize];
            visit(node);
            slot = node.next;
        }
    }

    /// Verifies every structural invariant. Intended for tests; O(book size).
    ///
    /// # Errors
    /// Returns a description of the first violated invariant.
    pub fn check_invariants(&self) -> Result<(), String> {
        let mut seen = 0usize;
        for (side, market, book) in [
            (Side::Buy, &self.market_bids, &self.bids),
            (Side::Sell, &self.market_asks, &self.asks),
        ] {
            if market.count > 0 && self.phase == Phase::Continuous {
                return Err(format!("{side} market orders rest in continuous trading"));
            }
            seen += self.check_queue(side, MARKET, market)?;
            for (&price, level) in book {
                if level.count == 0 {
                    return Err(format!("empty {side} level at {price}"));
                }
                seen += self.check_queue(side, price, level)?;
            }
        }
        if seen != self.index.len() {
            return Err(format!(
                "{} ids indexed but {seen} orders resting",
                self.index.len()
            ));
        }
        if seen + self.free.len() != self.nodes.len() {
            return Err("slab slots leaked".to_owned());
        }
        if let (Some(bid), Some(ask)) = (self.best_bid(), self.best_ask())
            && bid.price >= ask.price
            && self.phase == Phase::Continuous
        {
            return Err(format!(
                "crossed book: bid {} >= ask {}",
                bid.price, ask.price
            ));
        }
        if self.phase == Phase::Continuous && self.reference.is_some() {
            return Err("a reference price outside a call phase".to_owned());
        }
        Ok(())
    }

    /// Checks one level or market queue and returns its order count.
    fn check_queue(&self, side: Side, price: Price, level: &Level) -> Result<usize, String> {
        let (mut count, mut total, mut prev, mut last_priority) = (0u32, 0u64, NIL, 0u64);
        let mut slot = level.head;
        while slot != NIL {
            let node = &self.nodes[slot as usize];
            let id = node.id;
            if node.prev != prev {
                return Err(format!("order {id}: broken back link"));
            }
            if node.side != side || node.price != price {
                return Err(format!("order {id}: stored in the wrong level"));
            }
            if node.remaining == 0 || node.remaining > MAX_QTY {
                return Err(format!("order {id}: open quantity {}", node.remaining));
            }
            if node.priority <= last_priority {
                return Err(format!("order {id}: queue out of time priority"));
            }
            if node.post_only && self.phase == Phase::Call {
                return Err(format!("order {id}: post-only order in a call phase"));
            }
            if self.index.get(&id) != Some(&slot) {
                return Err(format!("order {id}: id index points elsewhere"));
            }
            last_priority = node.priority;
            count += 1;
            total += node.remaining;
            prev = slot;
            slot = node.next;
        }
        if level.tail != prev {
            return Err(format!("{side} level {price}: tail is not the last order"));
        }
        if count != level.count || total != level.total {
            return Err(format!(
                "{side} level {price}: cached count/total {}/{} but queue has {count}/{total}",
                level.count, level.total
            ));
        }
        Ok(count as usize)
    }
}

fn side_book<'a>(
    bids: &'a mut BTreeMap<Price, Level>,
    asks: &'a mut BTreeMap<Price, Level>,
    side: Side,
) -> &'a mut BTreeMap<Price, Level> {
    match side {
        Side::Buy => bids,
        Side::Sell => asks,
    }
}

const fn market_queue<'a>(bids: &'a mut Level, asks: &'a mut Level, side: Side) -> &'a mut Level {
    match side {
        Side::Buy => bids,
        Side::Sell => asks,
    }
}

fn link_back(level: &mut Level, nodes: &mut [Node], slot: u32) {
    let tail = level.tail;
    let node = &mut nodes[slot as usize];
    node.prev = tail;
    node.next = NIL;
    level.total += node.remaining;
    level.count += 1;
    if tail == NIL {
        level.head = slot;
    } else {
        nodes[tail as usize].next = slot;
    }
    level.tail = slot;
}

fn unlink(level: &mut Level, nodes: &mut [Node], slot: u32) {
    let Node {
        prev,
        next,
        remaining,
        ..
    } = nodes[slot as usize];
    if prev == NIL {
        level.head = next;
    } else {
        nodes[prev as usize].next = next;
    }
    if next == NIL {
        level.tail = prev;
    } else {
        nodes[next as usize].prev = prev;
    }
    level.total -= remaining;
    level.count -= 1;
}

const fn level_view(price: Price, level: &Level) -> LevelView {
    LevelView {
        price,
        qty: level.total,
        orders: level.count as usize,
    }
}

const fn order_view(node: &Node) -> OrderView {
    OrderView {
        id: node.id,
        account: node.account,
        side: node.side,
        price: node.price,
        remaining: node.remaining,
        stp: node.stp,
        post_only: node.post_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_is_compact() {
        assert_eq!(std::mem::size_of::<Node>(), 48);
        assert_eq!(std::mem::size_of::<Level>(), 24);
    }
}
