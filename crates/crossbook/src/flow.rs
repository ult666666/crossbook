//! Seeded synthetic order flow for tests, benchmarks and the browser playground.
//!
//! The generator keeps its own copy of the book (a shadow [`Engine`] fed with every
//! command it emits), so it behaves like a set of participants who can see the market:
//!
//! - passive limit orders cluster a few ticks around a drifting fair value;
//! - cancels and modifies target orders that are actually resting;
//! - the cancel rate rises when the book is deeper than a target and falls when it is
//!   thinner, so book size stays stable over long runs;
//! - a small share of aggressive limit, IOC, FOK and market orders cross the spread.
//!
//! Only integer arithmetic and its own PRNG are used, so a seed yields the same stream on
//! every platform, including WebAssembly. The flow is synthetic and not calibrated to any
//! real market; it exists to exercise every code path at a realistic mix.

use crate::Engine;
use crate::types::{
    AccountId, Command, Event, NewOrder, OrderId, OrderKind, Price, Qty, Side, StpMode, TimeInForce,
};

/// SplitMix64: a small, fast, well-tested 64-bit generator (Steele, Lea and Flood, 2014).
#[derive(Clone, Debug)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// A generator seeded with `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Next 64 random bits.
    pub const fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform value in `0..bound` by multiply-shift (Lemire). `bound` must be positive.
    pub const fn below(&mut self, bound: u64) -> u64 {
        ((self.next_u64() as u128 * bound as u128) >> 64) as u64
    }

    /// True with probability `per_mille / 1000`.
    pub const fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
}

/// Parameters of the synthetic flow. Shares are parts per thousand of all commands; the
/// remainder after cancels, modifies and aggressive orders is passive GTC limit orders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlowConfig {
    /// PRNG seed.
    pub seed: u64,
    /// Starting fair value in ticks.
    pub start_price: Price,
    /// Number of accounts. Self-trade prevention needs repeated owners.
    pub accounts: AccountId,
    /// Book size, in resting orders, that the cancel rate steers toward.
    pub target_orders: usize,
    /// Cancel share when the book is at its target size.
    pub cancel: u64,
    /// Modify share.
    pub modify: u64,
    /// Marketable GTC limit orders.
    pub aggressive: u64,
    /// Marketable IOC limit orders.
    pub ioc: u64,
    /// Marketable FOK limit orders.
    pub fok: u64,
    /// IOC market orders.
    pub market: u64,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            start_price: 10_000,
            accounts: 64,
            target_orders: 10_000,
            cancel: 380,
            modify: 60,
            aggressive: 25,
            ioc: 25,
            fok: 5,
            market: 10,
        }
    }
}

impl FlowConfig {
    /// The default mix with another seed.
    #[must_use]
    pub fn seeded(seed: u64) -> Self {
        Self {
            seed,
            ..Self::default()
        }
    }
}

/// Endless iterator of synthetic commands.
#[derive(Clone, Debug)]
pub struct Flow {
    config: FlowConfig,
    rng: SplitMix64,
    next_id: OrderId,
    fair: Price,
    shadow: Engine,
    live: Vec<OrderId>,
    scratch: Vec<Event>,
}

impl Flow {
    /// A generator for `config`.
    #[must_use]
    pub fn new(config: FlowConfig) -> Self {
        Self {
            config,
            rng: SplitMix64::new(config.seed),
            next_id: 1,
            fair: config.start_price,
            shadow: Engine::new(),
            live: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// The generator's own view of the book after every command emitted so far.
    #[must_use]
    pub const fn book(&self) -> &Engine {
        &self.shadow
    }

    /// Mostly small clips, sometimes a block.
    fn qty(&mut self) -> Qty {
        match self.rng.below(100) {
            0..70 => 1 + self.rng.below(10),
            70..95 => 10 + self.rng.below(40),
            _ => 50 + self.rng.below(200),
        }
    }

    /// Ticks behind the touch, geometrically distributed (most orders near the inside).
    fn depth_offset(&mut self) -> Price {
        let mut offset = 1;
        while offset < 50 && self.rng.chance(700) {
            offset += 1;
        }
        offset
    }

    fn account(&mut self) -> AccountId {
        1 + self.rng.below(u64::from(self.config.accounts.max(1))) as AccountId
    }

    fn stp(&mut self) -> StpMode {
        match self.rng.below(100) {
            0..70 => StpMode::Allow,
            70..85 => StpMode::CancelNewest,
            85..95 => StpMode::CancelOldest,
            _ => StpMode::CancelBoth,
        }
    }

    fn side(&mut self) -> Side {
        if self.rng.below(2) == 0 {
            Side::Buy
        } else {
            Side::Sell
        }
    }

    /// A random id that still rests in the shadow book, dropping stale ids on the way.
    fn pick_live(&mut self) -> Option<OrderId> {
        while !self.live.is_empty() {
            let index = self.rng.below(self.live.len() as u64) as usize;
            let id = self.live[index];
            if self.shadow.order(id).is_some() {
                return Some(id);
            }
            self.live.swap_remove(index);
        }
        None
    }

    /// Cancel share adjusted toward the target book size.
    fn cancel_share(&self) -> u64 {
        let target = self.config.target_orders.max(1);
        let len = self.shadow.len();
        let base = self.config.cancel;
        if len > target + target / 4 {
            base + base / 2
        } else if len > target {
            base + base / 5
        } else if len < target / 2 {
            base / 2
        } else {
            base
        }
    }

    /// A price `reach` ticks through the opposite touch, or through fair value if empty.
    fn marketable_price(&mut self, side: Side) -> Price {
        let reach = self.rng.below(3) as Price;
        match side {
            Side::Buy => {
                self.shadow
                    .best_ask()
                    .map_or(self.fair + 1, |level| level.price)
                    + reach
            }
            Side::Sell => (self
                .shadow
                .best_bid()
                .map_or(self.fair - 1, |level| level.price)
                - reach)
                .max(1),
        }
    }

    fn order(
        &mut self,
        side: Side,
        kind: OrderKind,
        tif: TimeInForce,
        post_only: bool,
    ) -> NewOrder {
        let id = self.next_id;
        self.next_id += 1;
        NewOrder {
            id,
            account: self.account(),
            side,
            kind,
            qty: self.qty(),
            tif,
            post_only,
            stp: self.stp(),
        }
    }

    fn generate(&mut self) -> Command {
        // Fair value drifts one tick at a time.
        match self.rng.below(1000) {
            0..3 => self.fair += 1,
            3..6 => self.fair = (self.fair - 1).max(10),
            _ => {}
        }
        let mut roll = self.rng.below(1000);
        let mut take = |share: u64| {
            let hit = roll < share;
            roll = roll.saturating_sub(share);
            hit
        };
        let cancel = take(self.cancel_share());
        let modify = !cancel && take(self.config.modify);
        if (cancel || modify)
            && let Some(id) = self.pick_live()
        {
            if cancel {
                return Command::Cancel { id };
            }
            let resting = self.shadow.order(id).expect("picked order rests");
            let price = match self.rng.below(4) {
                0 | 1 => resting.price,
                2 => resting.price + 1,
                _ => (resting.price - 1).max(1),
            };
            return Command::Modify {
                id,
                price,
                qty: self.qty(),
            };
        }

        let side = self.side();
        let order = if take(self.config.aggressive) {
            let price = self.marketable_price(side);
            self.order(side, OrderKind::Limit(price), TimeInForce::Gtc, false)
        } else if take(self.config.ioc) {
            let price = self.marketable_price(side);
            self.order(side, OrderKind::Limit(price), TimeInForce::Ioc, false)
        } else if take(self.config.fok) {
            let price = self.marketable_price(side);
            self.order(side, OrderKind::Limit(price), TimeInForce::Fok, false)
        } else if take(self.config.market) {
            self.order(side, OrderKind::Market, TimeInForce::Ioc, false)
        } else {
            // Passive: behind fair value and never through the opposite touch.
            let offset = self.depth_offset();
            let price = match side {
                Side::Buy => {
                    let ceiling = self
                        .shadow
                        .best_ask()
                        .map_or(Price::MAX, |level| level.price - 1);
                    (self.fair - offset).min(ceiling).max(1)
                }
                Side::Sell => {
                    let floor = self.shadow.best_bid().map_or(1, |level| level.price + 1);
                    (self.fair + offset).max(floor)
                }
            };
            let post_only = self.rng.chance(150);
            self.order(side, OrderKind::Limit(price), TimeInForce::Gtc, post_only)
        };
        if order.tif == TimeInForce::Gtc && order.kind != OrderKind::Market {
            self.live.push(order.id);
        }
        Command::New(order)
    }
}

impl Iterator for Flow {
    type Item = Command;

    fn next(&mut self) -> Option<Command> {
        let command = self.generate();
        self.scratch.clear();
        self.shadow.process(&command, &mut self.scratch);
        Some(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_matches_reference_output() {
        // First outputs for seed 1234567 from the reference C implementation.
        let mut rng = SplitMix64::new(1_234_567);
        assert_eq!(rng.next_u64(), 6_457_827_717_110_365_317);
        assert_eq!(rng.next_u64(), 3_203_168_211_198_807_973);
        assert_eq!(rng.next_u64(), 9_817_491_932_198_370_423);
    }

    #[test]
    fn same_seed_same_stream() {
        let a: Vec<_> = Flow::new(FlowConfig::seeded(7)).take(5_000).collect();
        let b: Vec<_> = Flow::new(FlowConfig::seeded(7)).take(5_000).collect();
        let c: Vec<_> = Flow::new(FlowConfig::seeded(8)).take(5_000).collect();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn book_size_stays_near_target() {
        let config = FlowConfig {
            target_orders: 2_000,
            ..FlowConfig::seeded(3)
        };
        let mut flow = Flow::new(config);
        for _ in 0..200_000 {
            flow.next();
        }
        let len = flow.book().len();
        assert!(
            (1_000..=3_000).contains(&len),
            "book drifted to {len} orders"
        );
    }
}
