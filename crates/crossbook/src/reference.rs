//! A deliberately simple second implementation of the same matching rules.
//!
//! [`ReferenceEngine`] keeps every resting order in one `Vec` and answers every question
//! with a linear scan: no trees, no linked lists, no cached totals. Fill-or-kill is
//! decided by running the order as immediate-or-cancel on a copy of the book. It is slow
//! on purpose, so its correctness is easy to see.
//!
//! The tests feed random command streams to both engines and require identical events
//! and identical books after every command (differential testing). A bug in the fast
//! engine's data structures shows up as a disagreement with this model.

use crate::types::{
    AccountId, CancelReason, Command, Depth, Event, LevelView, MAX_QTY, NewOrder, OrderId,
    OrderView, Price, Qty, RejectReason, Side, StpMode, TimeInForce, crosses, static_reject,
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
        }
    }

    fn submit(&mut self, order: &NewOrder, out: &mut Vec<Event>) {
        let limit = order.kind.limit();
        let reason = if let Some(reason) = static_reject(order) {
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
        out.push(Event::Booked {
            id: view.id,
            side: view.side,
            price: view.price,
            qty: view.remaining,
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
        orders.sort_by_key(|order| {
            (
                order.view.side == Side::Sell,
                price_rank(&order.view),
                order.priority,
            )
        });
        orders.into_iter().map(|order| order.view).collect()
    }

    /// Up to `levels` aggregated price levels per side, best first.
    #[must_use]
    pub fn depth(&self, levels: usize) -> Depth {
        let mut depth = Depth::default();
        for order in self.snapshot() {
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
