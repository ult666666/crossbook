//! WebAssembly bindings for the crossbook browser playground.
//!
//! The playground drives one [`Engine`] through a small, string-typed API and receives
//! JSON. Numbers stay well inside the range a JavaScript number represents exactly.
//! Timing happens in JavaScript (`performance.now()`), because `std::time::Instant` is
//! not available on `wasm32-unknown-unknown`.

use std::fmt::Write as _;

use crossbook::flow::{Flow, FlowConfig};
use crossbook::{
    CancelReason, Command, Engine, Event, NewOrder, OrderKind, RejectReason, Side, StpMode,
    TimeInForce, digest,
};
use wasm_bindgen::prelude::*;

/// Interactive book plus a separate engine for the stress test.
#[wasm_bindgen]
pub struct Playground {
    engine: Engine,
    events: Vec<Event>,
    next_id: u64,
    staged: Vec<Command>,
}

impl Default for Playground {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
impl Playground {
    /// An empty book.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: Engine::new(),
            events: Vec::new(),
            next_id: 1,
            staged: Vec::new(),
        }
    }

    /// Clears the interactive book and restarts ids at 1.
    pub fn reset(&mut self) {
        self.engine = Engine::new();
        self.next_id = 1;
    }

    /// Id the next submitted order will receive.
    #[wasm_bindgen(js_name = nextId)]
    #[must_use]
    pub fn next_id(&self) -> f64 {
        self.next_id as f64
    }

    /// Submits a new order and returns its events as a JSON array.
    ///
    /// `side`: `buy` | `sell`; `kind`: `limit` | `market`; `tif`: `gtc` | `ioc` | `fok`;
    /// `stp`: `allow` | `cancel-newest` | `cancel-oldest` | `cancel-both`.
    /// Unknown strings fall back to the first option. `price` is ignored for market orders.
    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &mut self,
        side: &str,
        kind: &str,
        price: i32,
        qty: u32,
        tif: &str,
        post_only: bool,
        account: u32,
        stp: &str,
    ) -> String {
        let id = self.next_id;
        self.next_id += 1;
        let order = NewOrder {
            id,
            account,
            side: if side == "sell" {
                Side::Sell
            } else {
                Side::Buy
            },
            kind: if kind == "market" {
                OrderKind::Market
            } else {
                OrderKind::Limit(i64::from(price))
            },
            qty: u64::from(qty),
            tif: match tif {
                "ioc" => TimeInForce::Ioc,
                "fok" => TimeInForce::Fok,
                _ => TimeInForce::Gtc,
            },
            post_only,
            stp: match stp {
                "cancel-newest" => StpMode::CancelNewest,
                "cancel-oldest" => StpMode::CancelOldest,
                "cancel-both" => StpMode::CancelBoth,
                _ => StpMode::Allow,
            },
        };
        self.apply(&Command::New(order))
    }

    /// Cancels a resting order and returns the events as JSON.
    pub fn cancel(&mut self, id: u32) -> String {
        self.apply(&Command::Cancel { id: u64::from(id) })
    }

    /// Modifies a resting order and returns the events as JSON.
    pub fn modify(&mut self, id: u32, price: i32, qty: u32) -> String {
        self.apply(&Command::Modify {
            id: u64::from(id),
            price: i64::from(price),
            qty: u64::from(qty),
        })
    }

    fn apply(&mut self, command: &Command) -> String {
        self.events.clear();
        self.engine.process(command, &mut self.events);
        let mut json = String::from("[");
        for (index, event) in self.events.iter().enumerate() {
            if index > 0 {
                json.push(',');
            }
            event_json(event, &mut json);
        }
        json.push(']');
        json
    }

    /// The book as JSON: up to `levels` levels per side, best first, each with its queue.
    #[must_use]
    pub fn book(&self, levels: usize) -> String {
        let depth = self.engine.depth(levels);
        let mut json = String::from("{\"bids\":");
        self.levels_json(Side::Buy, &depth.bids, &mut json);
        json.push_str(",\"asks\":");
        self.levels_json(Side::Sell, &depth.asks, &mut json);
        let _ = write!(
            json,
            ",\"orders\":{},\"bidLevels\":{},\"askLevels\":{},\"stateHash\":\"{:016x}\"}}",
            self.engine.len(),
            self.engine.level_count(Side::Buy),
            self.engine.level_count(Side::Sell),
            self.engine.state_hash()
        );
        json
    }

    fn levels_json(&self, side: Side, levels: &[crossbook::LevelView], json: &mut String) {
        json.push('[');
        for (index, level) in levels.iter().enumerate() {
            if index > 0 {
                json.push(',');
            }
            let _ = write!(
                json,
                "{{\"price\":{},\"qty\":{},\"queue\":[",
                level.price, level.qty
            );
            for (position, order) in self.engine.queue(side, level.price).iter().enumerate() {
                if position > 0 {
                    json.push(',');
                }
                let _ = write!(
                    json,
                    "{{\"id\":{},\"account\":{},\"qty\":{},\"postOnly\":{}}}",
                    order.id, order.account, order.remaining, order.post_only
                );
            }
            json.push_str("]}");
        }
        json.push(']');
    }

    /// Generates `count` synthetic commands for the stress test (not timed by the caller).
    pub fn stage(&mut self, count: u32, seed: u32) {
        self.staged = Flow::new(FlowConfig::seeded(u64::from(seed)))
            .take(count as usize)
            .collect();
    }

    /// Runs the staged commands through a fresh engine and returns a JSON summary.
    /// Time this call from JavaScript to measure engine throughput.
    #[wasm_bindgen(js_name = runStaged)]
    #[must_use]
    pub fn run_staged(&self) -> String {
        let mut engine = Engine::new();
        let mut events = Vec::with_capacity(64);
        let mut fingerprint = digest::Fingerprint::new();
        let (mut event_count, mut trades, mut volume) = (0u64, 0u64, 0u64);
        for command in &self.staged {
            events.clear();
            engine.process(command, &mut events);
            for event in &events {
                fingerprint.event(event);
                if let Event::Trade { qty, .. } = event {
                    trades += 1;
                    volume += qty;
                }
            }
            event_count += events.len() as u64;
        }
        format!(
            "{{\"commands\":{},\"events\":{event_count},\"trades\":{trades},\"volume\":{volume},\"resting\":{},\"eventDigest\":\"{:016x}\",\"stateHash\":\"{:016x}\"}}",
            self.staged.len(),
            engine.len(),
            fingerprint.value(),
            engine.state_hash()
        )
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn reject_name(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::InvalidQuantity => "invalid-quantity",
        RejectReason::InvalidPrice => "invalid-price",
        RejectReason::MarketOrderCannotRest => "market-order-cannot-rest",
        RejectReason::PostOnlyRequiresGtcLimit => "post-only-requires-gtc-limit",
        RejectReason::DuplicateOrderId => "duplicate-order-id",
        RejectReason::UnknownOrder => "unknown-order",
        RejectReason::PostOnlyWouldCross => "post-only-would-cross",
        RejectReason::FillOrKillUnfillable => "fill-or-kill-unfillable",
    }
}

fn cancel_name(reason: CancelReason) -> &'static str {
    match reason {
        CancelReason::Requested => "requested",
        CancelReason::Unfilled => "unfilled",
        CancelReason::SelfTrade => "self-trade",
    }
}

fn event_json(event: &Event, json: &mut String) {
    let _ = match *event {
        Event::Accepted {
            id,
            side,
            price,
            qty,
        } => match price {
            Some(price) => write!(
                json,
                "{{\"type\":\"accepted\",\"id\":{id},\"side\":\"{}\",\"price\":{price},\"qty\":{qty}}}",
                side_name(side)
            ),
            None => write!(
                json,
                "{{\"type\":\"accepted\",\"id\":{id},\"side\":\"{}\",\"price\":null,\"qty\":{qty}}}",
                side_name(side)
            ),
        },
        Event::Rejected { id, reason } => {
            write!(
                json,
                "{{\"type\":\"rejected\",\"id\":{id},\"reason\":\"{}\"}}",
                reject_name(reason)
            )
        }
        Event::Trade {
            taker,
            maker,
            taker_side,
            price,
            qty,
            taker_remaining,
            maker_remaining,
        } => write!(
            json,
            "{{\"type\":\"trade\",\"taker\":{taker},\"maker\":{maker},\"side\":\"{}\",\"price\":{price},\"qty\":{qty},\"takerRemaining\":{taker_remaining},\"makerRemaining\":{maker_remaining}}}",
            side_name(taker_side)
        ),
        Event::Booked {
            id,
            side,
            price,
            qty,
        } => write!(
            json,
            "{{\"type\":\"booked\",\"id\":{id},\"side\":\"{}\",\"price\":{price},\"qty\":{qty}}}",
            side_name(side)
        ),
        Event::Canceled {
            id,
            remaining,
            reason,
        } => write!(
            json,
            "{{\"type\":\"canceled\",\"id\":{id},\"remaining\":{remaining},\"reason\":\"{}\"}}",
            cancel_name(reason)
        ),
        Event::Modified {
            id,
            price,
            qty,
            kept_priority,
        } => write!(
            json,
            "{{\"type\":\"modified\",\"id\":{id},\"price\":{price},\"qty\":{qty},\"keptPriority\":{kept_priority}}}"
        ),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_returns_trade_json() {
        let mut playground = Playground::new();
        playground.submit("sell", "limit", 101, 5, "gtc", false, 1, "allow");
        let events = playground.submit("buy", "limit", 101, 3, "gtc", false, 2, "allow");
        assert_eq!(
            events,
            "[{\"type\":\"accepted\",\"id\":2,\"side\":\"buy\",\"price\":101,\"qty\":3},{\"type\":\"trade\",\"taker\":2,\"maker\":1,\"side\":\"buy\",\"price\":101,\"qty\":3,\"takerRemaining\":0,\"makerRemaining\":2}]"
        );
        assert!(playground.book(5).contains("\"asks\":[{\"price\":101,\"qty\":2,\"queue\":[{\"id\":1,\"account\":1,\"qty\":2,\"postOnly\":false}]}]"));
    }

    #[test]
    fn staged_run_matches_native_golden_digest() {
        let mut playground = Playground::new();
        playground.stage(100_000, 42);
        let summary = playground.run_staged();
        assert!(
            summary.contains("\"eventDigest\":\"3749b835b0ff0cae\""),
            "{summary}"
        );
        assert!(
            summary.contains("\"stateHash\":\"ee0bfb94bbfc39a7\""),
            "{summary}"
        );
    }
}
