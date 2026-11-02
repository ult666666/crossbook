//! Value types shared by the engine, the reference model and the wire codec.

use core::fmt;

/// Client-assigned order identifier. It must be unique among live (resting) orders.
pub type OrderId = u64;

/// Owner of an order. Self-trade prevention compares accounts.
pub type AccountId = u32;

/// Price in integer ticks. Limit prices must be positive.
pub type Price = i64;

/// Quantity in integer lots.
pub type Qty = u64;

/// Largest accepted quantity for one order or one modify.
///
/// Level totals are `u64`. The book holds fewer than `u32::MAX` orders, so capping each
/// order at `u32::MAX` lots makes overflow impossible.
pub const MAX_QTY: Qty = u32::MAX as Qty;

/// The trading phase of a book.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Continuous trading: an incoming order matches on arrival.
    #[default]
    Continuous,
    /// Auction call phase: orders collect without matching until an uncross.
    Call,
}

/// Order side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// Bid: wants to buy at the limit price or lower.
    Buy,
    /// Ask: wants to sell at the limit price or higher.
    Sell,
}

impl Side {
    /// The side an order of this side trades against.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

/// How an incoming order is priced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderKind {
    /// Trade at this price or better.
    Limit(Price),
    /// Trade at any price against resting liquidity. Never rests on the book.
    Market,
}

impl OrderKind {
    /// The limit price, or `None` for a market order.
    #[must_use]
    pub const fn limit(self) -> Option<Price> {
        match self {
            Self::Limit(price) => Some(price),
            Self::Market => None,
        }
    }
}

/// How long the unfilled part of an order stays live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimeInForce {
    /// Good till canceled: the remainder rests on the book.
    Gtc,
    /// Immediate or cancel: fill what is available now and cancel the rest.
    Ioc,
    /// Fill or kill: fill the whole quantity now, or reject with no effect on the book.
    Fok,
}

/// Self-trade prevention (STP): what happens when an incoming order would trade
/// against a resting order from the same account. The incoming order's mode applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StpMode {
    /// Let the two orders trade.
    Allow,
    /// Cancel the rest of the incoming (newest) order and stop matching.
    CancelNewest,
    /// Cancel the resting (oldest) order and keep matching.
    CancelOldest,
    /// Cancel the resting order and the rest of the incoming order.
    CancelBoth,
}

/// A request to enter a new order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NewOrder {
    /// Client-assigned identifier.
    pub id: OrderId,
    /// Owner, used by self-trade prevention.
    pub account: AccountId,
    /// Buy or sell.
    pub side: Side,
    /// Limit price or market.
    pub kind: OrderKind,
    /// Quantity in lots. Must be positive.
    pub qty: Qty,
    /// Lifetime of the unfilled remainder.
    pub tif: TimeInForce,
    /// Reject the order instead of letting it take liquidity.
    pub post_only: bool,
    /// Self-trade prevention mode.
    pub stp: StpMode,
}

impl NewOrder {
    /// A good-till-canceled limit order for account 0 with self-trade prevention off.
    #[must_use]
    pub const fn limit(id: OrderId, side: Side, price: Price, qty: Qty) -> Self {
        Self {
            id,
            account: 0,
            side,
            kind: OrderKind::Limit(price),
            qty,
            tif: TimeInForce::Gtc,
            post_only: false,
            stp: StpMode::Allow,
        }
    }

    /// An immediate-or-cancel market order for account 0 with self-trade prevention off.
    #[must_use]
    pub const fn market(id: OrderId, side: Side, qty: Qty) -> Self {
        Self {
            id,
            account: 0,
            side,
            kind: OrderKind::Market,
            qty,
            tif: TimeInForce::Ioc,
            post_only: false,
            stp: StpMode::Allow,
        }
    }

    /// Sets the owning account.
    #[must_use]
    pub const fn account(mut self, account: AccountId) -> Self {
        self.account = account;
        self
    }

    /// Sets the time in force.
    #[must_use]
    pub const fn tif(mut self, tif: TimeInForce) -> Self {
        self.tif = tif;
        self
    }

    /// Marks the order post-only.
    #[must_use]
    pub const fn post_only(mut self) -> Self {
        self.post_only = true;
        self
    }

    /// Sets the self-trade prevention mode.
    #[must_use]
    pub const fn stp(mut self, stp: StpMode) -> Self {
        self.stp = stp;
        self
    }
}

/// An input to the engine. Commands are processed one at a time, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    /// Enter a new order.
    New(NewOrder),
    /// Cancel a resting order.
    Cancel {
        /// Order to cancel.
        id: OrderId,
    },
    /// Change a resting order's price and remaining quantity.
    ///
    /// Same price with an equal or smaller quantity keeps time priority.
    /// Any other change loses priority and re-enters the order, which may then trade.
    Modify {
        /// Order to modify.
        id: OrderId,
        /// New limit price.
        price: Price,
        /// New remaining quantity.
        qty: Qty,
    },
    /// Start an auction call phase: orders collect without matching until an uncross.
    ///
    /// The auction's reference price is `reference` when it is given and positive,
    /// otherwise the price of the last trade. Resting post-only orders are canceled. In a
    /// call phase already, only the reference price changes.
    StartCall {
        /// Reference price for the uncross, such as the previous closing price.
        reference: Option<Price>,
    },
    /// Run the auction: find one clearing price and execute every crossing order at it.
    /// The book stays in the call phase.
    Uncross,
    /// Return to continuous trading. A book that is crossed or holds market orders is
    /// uncrossed first.
    Resume,
}

/// Why a command was rejected. A rejected command never changes the book.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RejectReason {
    /// Quantity was zero.
    InvalidQuantity,
    /// Limit price was zero or negative.
    InvalidPrice,
    /// A market order asked to rest (time in force GTC).
    MarketOrderCannotRest,
    /// Post-only was set on a market, IOC or FOK order.
    PostOnlyRequiresGtcLimit,
    /// Another live order already uses this id.
    DuplicateOrderId,
    /// No live order has this id.
    UnknownOrder,
    /// A post-only order would have taken liquidity.
    PostOnlyWouldCross,
    /// A fill-or-kill order could not be filled in full.
    FillOrKillUnfillable,
    /// An IOC, FOK or post-only order arrived during a call phase. Each needs immediate
    /// matching, which only continuous trading offers.
    NeedsContinuousTrading,
}

/// Why live quantity left the book or an order ended without resting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CancelReason {
    /// The client asked for the cancel.
    Requested,
    /// IOC or market remainder that found no more liquidity, including a market order's
    /// remainder after an uncross.
    Unfilled,
    /// Self-trade prevention.
    SelfTrade,
    /// A call phase started. Post-only orders cannot take part in an auction, where every
    /// trade takes liquidity.
    CallPhase,
}

/// An output of the engine. One command produces one or more events, in a fixed order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    /// A new order passed validation. Trades, if any, follow.
    Accepted {
        /// Order id.
        id: OrderId,
        /// Order side.
        side: Side,
        /// Limit price, or `None` for a market order.
        price: Option<Price>,
        /// Entered quantity.
        qty: Qty,
    },
    /// A command failed validation and had no effect.
    Rejected {
        /// Order id named by the command.
        id: OrderId,
        /// The first check that failed.
        reason: RejectReason,
    },
    /// An incoming order traded against a resting order at the resting order's price.
    Trade {
        /// Incoming (aggressive) order.
        taker: OrderId,
        /// Resting (passive) order.
        maker: OrderId,
        /// Side of the incoming order.
        taker_side: Side,
        /// Execution price: always the maker's price.
        price: Price,
        /// Executed quantity.
        qty: Qty,
        /// Incoming order quantity still unfilled after this trade.
        taker_remaining: Qty,
        /// Resting order quantity still open after this trade.
        maker_remaining: Qty,
    },
    /// The unfilled remainder of an order now rests on the book.
    Booked {
        /// Order id.
        id: OrderId,
        /// Order side.
        side: Side,
        /// Resting price.
        price: Price,
        /// Resting quantity.
        qty: Qty,
    },
    /// Open quantity was removed without trading.
    Canceled {
        /// Order id.
        id: OrderId,
        /// Quantity removed.
        remaining: Qty,
        /// Why it was removed.
        reason: CancelReason,
    },
    /// A resting order was changed.
    Modified {
        /// Order id.
        id: OrderId,
        /// New price.
        price: Price,
        /// New remaining quantity.
        qty: Qty,
        /// `true` when the order kept its place in the queue.
        kept_priority: bool,
    },
    /// A market order entered during a call phase now waits for the uncross. It has no
    /// price, so it belongs to no price level.
    MarketBooked {
        /// Order id.
        id: OrderId,
        /// Order side.
        side: Side,
        /// Waiting quantity.
        qty: Qty,
    },
    /// The book entered a call phase, or restarted one with a new reference price.
    CallStarted {
        /// The reference price the uncross will use, if any.
        reference: Option<Price>,
    },
    /// One fill of an uncross. Both orders execute at the clearing price.
    AuctionTrade {
        /// Buy order.
        buy: OrderId,
        /// Sell order.
        sell: OrderId,
        /// The clearing price.
        price: Price,
        /// Executed quantity.
        qty: Qty,
        /// Buy order quantity still open after this trade.
        buy_remaining: Qty,
        /// Sell order quantity still open after this trade.
        sell_remaining: Qty,
    },
    /// An uncross finished. The fields repeat its [`UncrossSummary`].
    Uncrossed {
        /// Clearing price, or `None` when nothing could execute.
        price: Option<Price>,
        /// Lots executed.
        volume: Qty,
        /// The side with unexecuted quantity at the clearing price, if any.
        imbalance_side: Option<Side>,
        /// Lots on that side that were willing to trade at the clearing price but did not.
        imbalance: Qty,
    },
    /// The call phase ended and continuous trading resumed.
    Resumed,
}

/// The result of auction price determination: what an uncross executes.
///
/// [`crate::Engine::indicative_uncross`] reports it during a call phase, as exchanges
/// publish an indicative price, volume and surplus, and [`Event::Uncrossed`] repeats it
/// when the uncross runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct UncrossSummary {
    /// Clearing price, or `None` when no order can execute.
    pub price: Option<Price>,
    /// Lots that execute at the clearing price.
    pub volume: Qty,
    /// The side with more quantity willing to trade at the price: `None` when both sides
    /// are equal or nothing executes.
    pub imbalance_side: Option<Side>,
    /// The surplus: lots on `imbalance_side` that are willing to trade at the price but do
    /// not execute.
    pub imbalance: Qty,
}

impl UncrossSummary {
    /// The [`Event::Uncrossed`] that reports this result.
    #[must_use]
    pub const fn event(self) -> Event {
        Event::Uncrossed {
            price: self.price,
            volume: self.volume,
            imbalance_side: self.imbalance_side,
            imbalance: self.imbalance,
        }
    }
}

/// Aggregated quantity at one price.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelView {
    /// Price of the level.
    pub price: Price,
    /// Total open quantity.
    pub qty: Qty,
    /// Number of resting orders.
    pub orders: usize,
}

/// Top of the book on both sides, best price first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Depth {
    /// Bid levels, highest price first.
    pub bids: Vec<LevelView>,
    /// Ask levels, lowest price first.
    pub asks: Vec<LevelView>,
}

/// One resting order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OrderView {
    /// Order id.
    pub id: OrderId,
    /// Owning account.
    pub account: AccountId,
    /// Side.
    pub side: Side,
    /// Resting price, or 0 for a market order waiting for an uncross. Limit prices are
    /// always positive, so 0 is never a limit.
    pub price: Price,
    /// Open quantity.
    pub remaining: Qty,
    /// Self-trade prevention mode used if the order is modified and re-enters.
    pub stp: StpMode,
    /// Post-only flag used if the order is modified.
    pub post_only: bool,
}

impl OrderView {
    /// True for a market order waiting in a call phase (its price is 0).
    #[must_use]
    pub const fn is_market(&self) -> bool {
        self.price == MARKET
    }
}

/// The price a market order rests at during a call phase. It is not a valid limit price.
pub(crate) const MARKET: Price = 0;

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Continuous => "continuous trading",
            Self::Call => "call phase",
        })
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        })
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidQuantity => "invalid quantity",
            Self::InvalidPrice => "invalid price",
            Self::MarketOrderCannotRest => "market order cannot rest",
            Self::PostOnlyRequiresGtcLimit => "post-only requires a GTC limit order",
            Self::DuplicateOrderId => "duplicate order id",
            Self::UnknownOrder => "unknown order",
            Self::PostOnlyWouldCross => "post-only order would cross",
            Self::FillOrKillUnfillable => "fill-or-kill order cannot fill in full",
            Self::NeedsContinuousTrading => "IOC, FOK and post-only orders need continuous trading",
        })
    }
}

impl fmt::Display for CancelReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Requested => "requested",
            Self::Unfilled => "unfilled",
            Self::SelfTrade => "self-trade prevention",
            Self::CallPhase => "post-only order removed by the call phase",
        })
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Accepted {
                id,
                side,
                price: Some(price),
                qty,
            } => {
                write!(f, "accepted #{id} {side} {qty} @ {price}")
            }
            Self::Accepted {
                id,
                side,
                price: None,
                qty,
            } => {
                write!(f, "accepted #{id} {side} {qty} @ market")
            }
            Self::Rejected { id, reason } => write!(f, "rejected #{id}: {reason}"),
            Self::Trade {
                taker,
                maker,
                price,
                qty,
                ..
            } => {
                write!(f, "trade {qty} @ {price} (taker #{taker}, maker #{maker})")
            }
            Self::Booked {
                id,
                side,
                price,
                qty,
            } => write!(f, "booked #{id} {side} {qty} @ {price}"),
            Self::Canceled {
                id,
                remaining,
                reason,
            } => {
                write!(f, "canceled #{id} {remaining} ({reason})")
            }
            Self::Modified {
                id,
                price,
                qty,
                kept_priority,
            } => {
                let priority = if kept_priority { "kept" } else { "lost" };
                write!(f, "modified #{id} to {qty} @ {price} (priority {priority})")
            }
            Self::MarketBooked { id, side, qty } => {
                write!(f, "booked #{id} {side} {qty} @ market for the uncross")
            }
            Self::CallStarted {
                reference: Some(reference),
            } => write!(f, "call phase started (reference price {reference})"),
            Self::CallStarted { reference: None } => {
                f.write_str("call phase started (no reference price)")
            }
            Self::AuctionTrade {
                buy,
                sell,
                price,
                qty,
                ..
            } => write!(
                f,
                "auction trade {qty} @ {price} (buy #{buy}, sell #{sell})"
            ),
            Self::Uncrossed { price: None, .. } => f.write_str("uncrossed: nothing could execute"),
            Self::Uncrossed {
                price: Some(price),
                volume,
                imbalance,
                imbalance_side,
            } => {
                write!(f, "uncrossed at {price}: {volume} lots")?;
                write_surplus(f, imbalance, imbalance_side)
            }
            Self::Resumed => f.write_str("continuous trading resumed"),
        }
    }
}

impl fmt::Display for UncrossSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.price {
            Some(price) => {
                write!(f, "{} lots @ {price}", self.volume)?;
                write_surplus(f, self.imbalance, self.imbalance_side)
            }
            None => f.write_str("no price, nothing crosses"),
        }
    }
}

fn write_surplus(f: &mut fmt::Formatter<'_>, imbalance: Qty, side: Option<Side>) -> fmt::Result {
    match side {
        Some(side) => write!(f, ", surplus {imbalance} {side}"),
        None => f.write_str(", no surplus"),
    }
}

/// True when an incoming order on `side` with `limit` may trade at `resting_price`.
#[inline]
pub(crate) const fn crosses(side: Side, limit: Option<Price>, resting_price: Price) -> bool {
    match (side, limit) {
        (_, None) => true,
        (Side::Buy, Some(limit)) => resting_price <= limit,
        (Side::Sell, Some(limit)) => resting_price >= limit,
    }
}

/// First check that `order` fails without looking at the book, given the trading phase.
pub(crate) const fn static_reject(order: &NewOrder, phase: Phase) -> Option<RejectReason> {
    if order.qty == 0 || order.qty > MAX_QTY {
        return Some(RejectReason::InvalidQuantity);
    }
    if let OrderKind::Limit(price) = order.kind
        && price <= 0
    {
        return Some(RejectReason::InvalidPrice);
    }
    let gtc = matches!(order.tif, TimeInForce::Gtc);
    match phase {
        // A call phase has no immediate matching, so only plain GTC orders join it. A GTC
        // market order waits for the uncross.
        Phase::Call if order.post_only || !gtc => Some(RejectReason::NeedsContinuousTrading),
        Phase::Call => None,
        Phase::Continuous if gtc && matches!(order.kind, OrderKind::Market) => {
            Some(RejectReason::MarketOrderCannotRest)
        }
        Phase::Continuous
            if order.post_only && (matches!(order.kind, OrderKind::Market) || !gtc) =>
        {
            Some(RejectReason::PostOnlyRequiresGtcLimit)
        }
        Phase::Continuous => None,
    }
}
