//! A deliberately simple second implementation of the same matching rules.
//!
//! [`ReferenceEngine`] keeps every resting order in one `Vec` and answers every question
//! with a linear scan: no trees, no linked lists, no cached totals. Fill-or-kill is
//! decided by running the order as immediate-or-cancel on a copy of the book, and the
//! auction price by [`auction_price`], which tries every candidate price. It is slow on
//! purpose, so its correctness is easy to see.
//!
//! The tests feed random command streams to both engines and require identical events
//! and identical books after every command (differential testing). A bug in the fast
//! engine's data structures shows up as a disagreement with this model.

use crate::types::{
    AccountId, CancelReason, Command, Depth, Event, LevelView, MARKET, MAX_QTY, NewOrder, OrderId,
    OrderView, Phase, Price, Qty, RejectReason, Side, StpMode, TimeInForce, UncrossSummary,
    crosses, static_reject,
};

#[derive(Clone, Copy, Debug)]
struct Resting {
    view: OrderView,
    priority: u64,
}

/// Linear-scan engine with the same semantics as [`crate::Engine`].
#[derive(Clone, Debug, Default)]
pub struct ReferenceEngine {
    resting: Vec<Resting>,
    clock: u64,
    phase: Phase,
    reference: Option<Price>,
    last_trade: Option<Price>,
}

/// One order as [`auction_price`] sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuctionOrder {
    /// Buy or sell.
    pub side: Side,
    /// Limit price, or `None` for a market order.
    pub limit: Option<Price>,
    /// Open quantity.
    pub qty: Qty,
}

/// The rule that settled an auction price, as [`auction_price`] reports it. Tests use it to
/// confirm that random books reach every rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Decided {
    /// No candidate can execute anything: there is no price.
    NoTrade,
    /// Market orders only: the reference price.
    MarketOnly,
    /// Rule 1, the single candidate with the highest executable volume.
    Volume,
    /// Rule 2, the lowest surplus among them.
    Surplus,
    /// Rule 3, market pressure: every tied candidate has its surplus on the same side.
    MarketPressure,
    /// Rule 4, the candidate closest to the reference price.
    Reference,
    /// Rule 5, the lowest remaining candidate.
    LowestPrice,
}

/// Brute-force auction price determination: evaluates every distinct limit price and
/// applies the rules in [`crate::Command::Uncross`] literally, keeping only the
/// candidates that tie after each rule.
#[must_use]
pub fn auction_price(
    orders: &[AuctionOrder],
    reference: Option<Price>,
) -> (UncrossSummary, Decided) {
    let quantity = |side: Side, price: Price| -> Qty {
        orders
            .iter()
            .filter(|order| order.side == side && crosses(side, order.limit, price))
            .map(|order| order.qty)
            .sum()
    };
    let market = |side: Side| -> Qty {
        orders
            .iter()
            .filter(|order| order.side == side && order.limit.is_none())
            .map(|order| order.qty)
            .sum()
    };
    let summary = |price: Price, demand: Qty, supply: Qty| UncrossSummary {
        price: Some(price),
        volume: demand.min(supply),
        imbalance_side: match demand.cmp(&supply) {
            core::cmp::Ordering::Greater => Some(Side::Buy),
            core::cmp::Ordering::Less => Some(Side::Sell),
            core::cmp::Ordering::Equal => None,
        },
        imbalance: demand.abs_diff(supply),
    };

    let mut prices: Vec<Price> = orders.iter().filter_map(|order| order.limit).collect();
    prices.sort_unstable();
    prices.dedup();
    if prices.is_empty() {
        let (demand, supply) = (market(Side::Buy), market(Side::Sell));
        return match reference {
            Some(price) if demand > 0 && supply > 0 => {
                (summary(price, demand, supply), Decided::MarketOnly)
            }
            _ => (UncrossSummary::default(), Decided::NoTrade),
        };
    }
    // For each candidate: the buyers willing to pay it and the sellers willing to accept it.
    let candidates: Vec<UncrossSummary> = prices
        .iter()
        .map(|&price| {
            // An order "crosses" a price when it would trade there as a taker.
            let demand = quantity(Side::Buy, price);
            let supply = quantity(Side::Sell, price);
            summary(price, demand, supply)
        })
        .collect();

    let most = candidates.iter().map(|c| c.volume).max().unwrap_or(0);
    if most == 0 {
        return (UncrossSummary::default(), Decided::NoTrade);
    }
    let rule1: Vec<_> = candidates.iter().filter(|c| c.volume == most).collect();
    if let [only] = rule1[..] {
        return (*only, Decided::Volume);
    }
    let least = rule1.iter().map(|c| c.imbalance).min().unwrap_or(0);
    let rule2: Vec<_> = rule1.into_iter().filter(|c| c.imbalance == least).collect();
    if let [only] = rule2[..] {
        return (*only, Decided::Surplus);
    }
    let all = |side| rule2.iter().all(|c| c.imbalance_side == Some(side));
    if all(Side::Buy) {
        let highest = rule2
            .iter()
            .max_by_key(|c| c.price)
            .expect("two or more candidates");
        return (**highest, Decided::MarketPressure);
    }
    if all(Side::Sell) {
        let lowest = rule2
            .iter()
            .min_by_key(|c| c.price)
            .expect("two or more candidates");
        return (**lowest, Decided::MarketPressure);
    }
    let rule4: Vec<_> = match reference {
        Some(reference) => {
            let distance = |c: &UncrossSummary| c.price.map(|p| p.abs_diff(reference));
            let nearest = rule2.iter().map(|c| distance(c)).min().flatten();
            rule2
                .into_iter()
                .filter(|c| distance(c) == nearest)
                .collect()
        }
        None => rule2,
    };
    if let [only] = rule4[..] {
        return (*only, Decided::Reference);
    }
    let lowest = rule4
        .iter()
        .min_by_key(|c| c.price)
        .expect("two or more candidates");
    (**lowest, Decided::LowestPrice)
}

impl ReferenceEngine {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one command and appends its events to `out`.
    pub fn process(&mut self, command: &Command, out: &mut Vec<Event>) {
        match *command {
            Command::New(ref order) => self.submit(order, out),
            Command::Cancel { id } => match self.position(id) {
                Some(index) => {
                    let removed = self.resting.remove(index).view;
                    out.push(Event::Canceled {
                        id,
                        remaining: removed.remaining,
                        reason: CancelReason::Requested,
                    });
                }
                None => out.push(Event::Rejected {
                    id,
                    reason: RejectReason::UnknownOrder,
                }),
            },
            Command::Modify { id, price, qty } => self.modify(id, price, qty, out),
            Command::StartCall { reference } => {
                self.phase = Phase::Call;
                self.reference = match reference {
                    Some(price) if price > 0 => Some(price),
                    _ => self.last_trade,
                };
                out.push(Event::CallStarted {
                    reference: self.reference,
                });
                for order in self.snapshot() {
                    if order.post_only {
                        let index = self.position(order.id).expect("listed order rests");
                        self.resting.remove(index);
                        out.push(Event::Canceled {
                            id: order.id,
                            remaining: order.remaining,
                            reason: CancelReason::CallPhase,
                        });
                    }
                }
            }
            Command::Uncross => self.uncross(out),
            Command::Resume => {
                let crossed = self.resting.iter().any(|bid| {
                    bid.view.side == Side::Buy
                        && self.resting.iter().any(|ask| {
                            ask.view.side == Side::Sell
                                && !bid.view.is_market()
                                && !ask.view.is_market()
                                && ask.view.price <= bid.view.price
                        })
                });
                if crossed || self.resting.iter().any(|order| order.view.is_market()) {
                    self.uncross(out);
                }
                self.phase = Phase::Continuous;
                self.reference = None;
                out.push(Event::Resumed);
            }
        }
    }

    /// The auction: trade the best remaining buy with the best remaining sell at one
    /// price until a side runs out, then cancel what is left of the market orders.
    fn uncross(&mut self, out: &mut Vec<Event>) {
        let summary = self.indicative_uncross();
        if let Some(price) = summary.price {
            let best = |engine: &Self, side: Side| {
                engine
                    .resting
                    .iter()
                    .enumerate()
                    .filter(|(_, order)| {
                        order.view.side == side
                            && (order.view.is_market()
                                || crosses(side.opposite(), Some(price), order.view.price))
                    })
                    .min_by_key(|(_, order)| auction_rank(order))
                    .map(|(index, _)| index)
            };
            while let (Some(buy), Some(sell)) = (best(self, Side::Buy), best(self, Side::Sell)) {
                let qty = self.resting[buy]
                    .view
                    .remaining
                    .min(self.resting[sell].view.remaining);
                self.resting[buy].view.remaining -= qty;
                self.resting[sell].view.remaining -= qty;
                let (b, s) = (self.resting[buy].view, self.resting[sell].view);
                out.push(Event::AuctionTrade {
                    buy: b.id,
                    sell: s.id,
                    price,
                    qty,
                    buy_remaining: b.remaining,
                    sell_remaining: s.remaining,
                });
                self.last_trade = Some(price);
                self.resting.retain(|order| order.view.remaining > 0);
            }
        }
        let mut market: Vec<Resting> = self
            .resting
            .iter()
            .filter(|order| order.view.is_market())
            .copied()
            .collect();
        market.sort_by_key(|order| (order.view.side == Side::Sell, order.priority));
        for order in market {
            out.push(Event::Canceled {
                id: order.view.id,
                remaining: order.view.remaining,
                reason: CancelReason::Unfilled,
            });
        }
        self.resting.retain(|order| !order.view.is_market());
        out.push(summary.event());
    }

    fn submit(&mut self, order: &NewOrder, out: &mut Vec<Event>) {
        let limit = order.kind.limit();
        let reason = if let Some(reason) = static_reject(order, self.phase) {
            Some(reason)
        } else if self.position(order.id).is_some() {
            Some(RejectReason::DuplicateOrderId)
        } else if order.post_only && self.best_maker(order.side, limit).is_some() {
            Some(RejectReason::PostOnlyWouldCross)
        } else if order.tif == TimeInForce::Fok && !self.fills_in_full(order) {
            Some(RejectReason::FillOrKillUnfillable)
        } else {
            None
        };
        if let Some(reason) = reason {
            out.push(Event::Rejected {
                id: order.id,
                reason,
            });
            return;
        }

        out.push(Event::Accepted {
            id: order.id,
            side: order.side,
            price: limit,
            qty: order.qty,
        });
        if self.phase == Phase::Call {
            self.rest(
                OrderView {
                    id: order.id,
                    account: order.account,
                    side: order.side,
                    price: limit.unwrap_or(MARKET),
                    remaining: order.qty,
                    stp: order.stp,
                    post_only: false,
                },
                out,
            );
            return;
        }
        let mut remaining = order.qty;
        let canceled = self.take(
            order.id,
            order.account,
            order.side,
            limit,
            order.stp,
            &mut remaining,
            out,
        );
        if canceled || remaining == 0 {
            return;
        }
        match (limit, order.tif) {
            (Some(price), TimeInForce::Gtc) => self.rest(
                OrderView {
                    id: order.id,
                    account: order.account,
                    side: order.side,
                    price,
                    remaining,
                    stp: order.stp,
                    post_only: order.post_only,
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

    /// Fill-or-kill: try the order as immediate-or-cancel on a throwaway copy.
    fn fills_in_full(&self, order: &NewOrder) -> bool {
        let mut trial = self.clone();
        let mut remaining = order.qty;
        let canceled = trial.take(
            order.id,
            order.account,
            order.side,
            order.kind.limit(),
            order.stp,
            &mut remaining,
            &mut Vec::new(),
        );
        !canceled && remaining == 0
    }

    fn modify(&mut self, id: OrderId, price: Price, qty: Qty, out: &mut Vec<Event>) {
        let Some(index) = self.position(id) else {
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
        let order = self.resting[index].view;
        if price == order.price && qty <= order.remaining {
            self.resting[index].view.remaining = qty;
            out.push(Event::Modified {
                id,
                price,
                qty,
                kept_priority: true,
            });
            return;
        }
        if order.post_only && self.best_maker(order.side, Some(price)).is_some() {
            out.push(Event::Rejected {
                id,
                reason: RejectReason::PostOnlyWouldCross,
            });
            return;
        }
        self.resting.remove(index);
        out.push(Event::Modified {
            id,
            price,
            qty,
            kept_priority: false,
        });
        if self.phase == Phase::Call {
            self.rest(
                OrderView {
                    price,
                    remaining: qty,
                    ..order
                },
                out,
            );
            return;
        }
        let mut remaining = qty;
        let canceled = self.take(
            id,
            order.account,
            order.side,
            Some(price),
            order.stp,
            &mut remaining,
            out,
        );
        if !canceled && remaining > 0 {
            self.rest(
                OrderView {
                    price,
                    remaining,
                    ..order
                },
                out,
            );
        }
    }

    /// Trades an incoming order against the book. Returns `true` if self-trade
    /// prevention canceled the incoming order.
    #[allow(clippy::too_many_arguments)]
    fn take(
        &mut self,
        id: OrderId,
        account: AccountId,
        side: Side,
        limit: Option<Price>,
        stp: StpMode,
        remaining: &mut Qty,
        out: &mut Vec<Event>,
    ) -> bool {
        while *remaining > 0 {
            let Some(index) = self.best_maker(side, limit) else {
                break;
            };
            let maker = self.resting[index].view;
            if stp != StpMode::Allow && maker.account == account {
                if matches!(stp, StpMode::CancelOldest | StpMode::CancelBoth) {
                    self.resting.remove(index);
                    out.push(Event::Canceled {
                        id: maker.id,
                        remaining: maker.remaining,
                        reason: CancelReason::SelfTrade,
                    });
                }
                if matches!(stp, StpMode::CancelNewest | StpMode::CancelBoth) {
                    out.push(Event::Canceled {
                        id,
                        remaining: *remaining,
                        reason: CancelReason::SelfTrade,
                    });
                    *remaining = 0;
                    return true;
                }
                continue;
            }
            let qty = (*remaining).min(maker.remaining);
            *remaining -= qty;
            self.resting[index].view.remaining -= qty;
            self.last_trade = Some(maker.price);
            let maker_remaining = self.resting[index].view.remaining;
            out.push(Event::Trade {
                taker: id,
                maker: maker.id,
                taker_side: side,
                price: maker.price,
                qty,
                taker_remaining: *remaining,
                maker_remaining,
            });
            if maker_remaining == 0 {
                self.resting.remove(index);
            }
        }
        false
    }

    fn rest(&mut self, view: OrderView, out: &mut Vec<Event>) {
        self.clock += 1;
        self.resting.push(Resting {
            view,
            priority: self.clock,
        });
        out.push(if view.is_market() {
            Event::MarketBooked {
                id: view.id,
                side: view.side,
                qty: view.remaining,
            }
        } else {
            Event::Booked {
                id: view.id,
                side: view.side,
                price: view.price,
                qty: view.remaining,
            }
        });
    }

    fn position(&self, id: OrderId) -> Option<usize> {
        self.resting.iter().position(|order| order.view.id == id)
    }

    /// Index of the resting order an incoming `side` order at `limit` would hit first:
    /// best price, then earliest priority.
    fn best_maker(&self, side: Side, limit: Option<Price>) -> Option<usize> {
        self.resting
            .iter()
            .enumerate()
            .filter(|(_, order)| {
                order.view.side == side.opposite() && crosses(side, limit, order.view.price)
            })
            .min_by_key(|(_, order)| (price_rank(&order.view), order.priority))
            .map(|(index, _)| index)
    }

    /// Every resting order in the same canonical order as [`crate::Engine::snapshot`].
    #[must_use]
    pub fn snapshot(&self) -> Vec<OrderView> {
        let mut orders = self.resting.clone();
        orders.sort_by_key(|order| (order.view.side == Side::Sell, auction_rank(order)));
        orders.into_iter().map(|order| order.view).collect()
    }

    /// The auction price determination of [`auction_price`] on the current book.
    #[must_use]
    pub fn indicative_uncross(&self) -> UncrossSummary {
        let orders: Vec<AuctionOrder> = self
            .resting
            .iter()
            .map(|order| AuctionOrder {
                side: order.view.side,
                limit: (!order.view.is_market()).then_some(order.view.price),
                qty: order.view.remaining,
            })
            .collect();
        auction_price(&orders, self.reference).0
    }

    /// The current trading phase.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// The reference price of the current call phase.
    #[must_use]
    pub const fn reference_price(&self) -> Option<Price> {
        self.reference
    }

    /// The price of the most recent trade.
    #[must_use]
    pub const fn last_trade_price(&self) -> Option<Price> {
        self.last_trade
    }

    /// Market orders waiting for an uncross on one side, in time priority.
    #[must_use]
    pub fn market_orders(&self, side: Side) -> Vec<OrderView> {
        self.snapshot()
            .into_iter()
            .filter(|order| order.side == side && order.is_market())
            .collect()
    }

    /// Up to `levels` aggregated price levels per side, best first.
    #[must_use]
    pub fn depth(&self, levels: usize) -> Depth {
        let mut depth = Depth::default();
        for order in self
            .snapshot()
            .into_iter()
            .filter(|order| !order.is_market())
        {
            let side = match order.side {
                Side::Buy => &mut depth.bids,
                Side::Sell => &mut depth.asks,
            };
            match side.last_mut() {
                Some(level) if level.price == order.price => {
                    level.qty += order.remaining;
                    level.orders += 1;
                }
                _ => side.push(LevelView {
                    price: order.price,
                    qty: order.remaining,
                    orders: 1,
                }),
            }
        }
        depth.bids.truncate(levels);
        depth.asks.truncate(levels);
        depth
    }

    /// Number of resting orders.
    #[must_use]
    pub fn len(&self) -> usize {
        self.resting.len()
    }

    /// True when the book is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.resting.is_empty()
    }
}

/// Sort key that puts the most aggressive price first: high bids, low asks.
fn price_rank(order: &OrderView) -> i128 {
    match order.side {
        Side::Buy => -i128::from(order.price),
        Side::Sell => i128::from(order.price),
    }
}

/// Priority within one side: market orders, then the best price, then the earliest order.
fn auction_rank(order: &Resting) -> (bool, i128, u64) {
    (
        !order.view.is_market(),
        price_rank(&order.view),
        order.priority,
    )
}
