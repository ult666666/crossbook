//! # crossbook
//!
//! A deterministic limit order book and matching engine for one instrument.
//!
//! - **Price-time priority.** Better prices trade first; within a price, earlier orders
//!   trade first. Trades execute at the resting order's price.
//! - **Order types.** Limit and market orders; good-till-canceled, immediate-or-cancel and
//!   fill-or-kill; post-only; cancel; modify with explicit priority rules; four
//!   self-trade prevention modes.
//! - **Call auctions.** A call phase collects orders without matching; the uncross picks
//!   one clearing price by maximum volume, minimum surplus, market pressure and the
//!   reference price, and executes every crossing order at it in price-time priority.
//! - **Deterministic.** No clocks, randomness or hash-map iteration in the matching path.
//!   The same commands always produce the same events, on any platform.
//! - **Checked two ways.** [`reference::ReferenceEngine`] implements the same rules with
//!   linear scans; property tests require both engines to agree on every event.
//!
//! Around the engine, built on its event stream and its determinism:
//!
//! - [`snapshot`] and [`recovery`]: checksummed snapshots and a write-ahead journal that
//!   rebuild the book exactly after a crash.
//! - [`market_data`]: trade prints and price-level updates derived from events, and a
//!   subscriber that detects gaps and resynchronizes.
//! - [`exchange`]: many instruments on several threads, with output identical to one
//!   thread.
//!
//! ```
//! use crossbook::{CancelReason, Command, Engine, Event, NewOrder, Side, TimeInForce};
//!
//! let mut engine = Engine::new();
//! let mut events = Vec::new();
//! engine.process(&Command::New(NewOrder::limit(1, Side::Sell, 100, 10)), &mut events);
//!
//! // An IOC buy for 15 fills 10 and cancels the other 5 instead of resting.
//! let ioc = NewOrder::limit(2, Side::Buy, 100, 15).tif(TimeInForce::Ioc);
//! events.clear();
//! engine.process(&Command::New(ioc), &mut events);
//! assert_eq!(
//!     events.last(),
//!     Some(&Event::Canceled { id: 2, remaining: 5, reason: CancelReason::Unfilled })
//! );
//! assert!(engine.is_empty());
//! ```
//!
//! A call auction: orders cross without trading until the uncross, which executes them at
//! one price.
//!
//! ```
//! use crossbook::{Command, Engine, NewOrder, Side, UncrossSummary};
//!
//! let mut engine = Engine::new();
//! let mut events = Vec::new();
//! engine.process(&Command::StartCall { reference: Some(100) }, &mut events);
//! engine.process(&Command::New(NewOrder::limit(1, Side::Buy, 101, 8)), &mut events);
//! engine.process(&Command::New(NewOrder::limit(2, Side::Sell, 100, 5)), &mut events);
//!
//! // 5 lots can trade at 100 or 101, with 3 left to buy at both: buyers press, so 101.
//! let summary = UncrossSummary { price: Some(101), volume: 5, imbalance_side: Some(Side::Buy), imbalance: 3 };
//! assert_eq!(engine.indicative_uncross(), summary);
//! engine.process(&Command::Uncross, &mut events);
//! engine.process(&Command::Resume, &mut events);
//! assert_eq!(engine.order(1).map(|order| order.remaining), Some(3));
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod auction;
pub mod codec;
pub mod digest;
mod engine;
pub mod exchange;
pub mod flow;
pub mod journal;
pub mod market_data;
pub mod recovery;
pub mod reference;
pub mod snapshot;
mod types;

pub use engine::Engine;
pub use types::{
    AccountId, CancelReason, Command, Depth, Event, LevelView, MAX_QTY, NewOrder, OrderId,
    OrderKind, OrderView, Phase, Price, Qty, RejectReason, Side, StpMode, TimeInForce,
    UncrossSummary,
};
