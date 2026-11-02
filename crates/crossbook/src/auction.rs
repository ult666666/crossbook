//! Call auctions: one clearing price for a book that has collected orders without matching.
//!
//! During a call phase ([`crate::Command::StartCall`]) orders rest without trading, so the
//! book may cross. The uncross ([`crate::Command::Uncross`]) picks a single clearing price
//! and executes every crossing order at it. For a candidate price `p`:
//!
//! - demand `D(p)` is the quantity of market buys plus bids with a limit at or above `p`;
//! - supply `S(p)` is the quantity of market sells plus asks with a limit at or below `p`;
//! - the executable volume is `min(D(p), S(p))` and the surplus is `|D(p) - S(p)|`, on the
//!   buy side when demand is larger and on the sell side when supply is.
//!
//! The candidates are the distinct limit prices on the book, and the rules apply in order,
//! each one only to the candidates that tie under the rules before it:
//!
//! 1. the highest executable volume;
//! 2. the lowest surplus;
//! 3. market pressure: if every remaining candidate has a buy surplus, the highest price;
//!    if every one has a sell surplus, the lowest;
//! 4. the price closest to the reference price (the price given with the call, otherwise
//!    the last trade);
//! 5. the lowest remaining price.
//!
//! If the highest volume is zero, nothing executes. A book with market orders on both
//! sides and no limit orders at all has no candidate; it clears at the reference price
//! when there is one. The rules follow the auction price determination of Deutsche
//! Börse's Xetra market model and the Euronext cash trading manual; `docs/DESIGN.md`
//! cites both and explains where crossbook differs.
//!
//! [`determine`] evaluates every candidate in one pass over the aggregated price levels, in
//! O(levels) time and constant extra memory, so the engine and the market-data publisher
//! can both call it. [`crate::reference::auction_price`] implements the same rules by
//! brute force, and the tests require the two to agree.

use crate::types::{Price, Qty, Side, UncrossSummary};

/// The candidates tied so far under rules 1 and 2, with what rules 3 to 5 need to choose
/// among them. Candidates arrive in ascending price order.
struct Ties {
    volume: Qty,
    surplus: Qty,
    lowest: (Price, Option<Side>),
    highest: (Price, Option<Side>),
    buy_surplus: bool,
    sell_surplus: bool,
    /// The candidate closest to the reference price, the lower one on an exact tie.
    nearest: Option<(u64, Price, Option<Side>)>,
}

impl Ties {
    fn new(price: Price, side: Option<Side>, volume: Qty, surplus: Qty) -> Self {
        Self {
            volume,
            surplus,
            lowest: (price, side),
            highest: (price, side),
            buy_surplus: false,
            sell_surplus: false,
            nearest: None,
        }
    }

    fn add(&mut self, price: Price, side: Option<Side>, reference: Option<Price>) {
        self.highest = (price, side);
        match side {
            Some(Side::Buy) => self.buy_surplus = true,
            Some(Side::Sell) => self.sell_surplus = true,
            None => {}
        }
        if let Some(reference) = reference {
            let distance = price.abs_diff(reference);
            if self.nearest.is_none_or(|(best, _, _)| distance < best) {
                self.nearest = Some((distance, price, side));
            }
        }
    }
}

/// Auction price determination over aggregated price levels.
///
/// `bids` and `asks` yield `(price, total quantity)` per level in ascending price order;
/// `market_buy` and `market_sell` are the total quantities of waiting market orders, which
/// count at every price.
pub(crate) fn determine<B, A>(
    bids: B,
    asks: A,
    market_buy: Qty,
    market_sell: Qty,
    reference: Option<Price>,
) -> UncrossSummary
where
    B: Iterator<Item = (Price, Qty)> + Clone,
    A: Iterator<Item = (Price, Qty)>,
{
    let bid_total: Qty = bids.clone().map(|(_, qty)| qty).sum();
    let mut bids = bids.peekable();
    let mut asks = asks.peekable();
    // Bid lots priced below the candidate, and ask lots priced at or below it.
    let (mut bids_below, mut asks_through) = (0, 0);
    let mut ties: Option<Ties> = None;
    loop {
        let price = match (bids.peek(), asks.peek()) {
            (None, None) => break,
            (Some(&(bid, _)), None) => bid,
            (None, Some(&(ask, _))) => ask,
            (Some(&(bid, _)), Some(&(ask, _))) => bid.min(ask),
        };
        let bid_here = bids
            .next_if(|&(bid, _)| bid == price)
            .map_or(0, |(_, qty)| qty);
        asks_through += asks
            .next_if(|&(ask, _)| ask == price)
            .map_or(0, |(_, qty)| qty);
        let demand = market_buy + (bid_total - bids_below);
        let supply = market_sell + asks_through;
        bids_below += bid_here;

        let volume = demand.min(supply);
        let surplus = demand.abs_diff(supply);
        let side = surplus_side(demand, supply);
        match &mut ties {
            Some(best) if volume < best.volume => {}
            Some(best) if volume == best.volume && surplus > best.surplus => {}
            Some(best) if volume == best.volume && surplus == best.surplus => {
                best.add(price, side, reference);
            }
            _ => {
                let mut best = Ties::new(price, side, volume, surplus);
                best.add(price, side, reference);
                ties = Some(best);
            }
        }
    }

    let Some(ties) = ties else {
        // No limit orders at all: market orders on both sides trade at the reference price.
        return match reference {
            Some(price) if market_buy > 0 && market_sell > 0 => UncrossSummary {
                price: Some(price),
                volume: market_buy.min(market_sell),
                imbalance_side: surplus_side(market_buy, market_sell),
                imbalance: market_buy.abs_diff(market_sell),
            },
            _ => UncrossSummary::default(),
        };
    };
    if ties.volume == 0 {
        return UncrossSummary::default();
    }
    let (price, side) = if ties.surplus > 0 && !ties.sell_surplus {
        ties.highest
    } else if ties.surplus > 0 && !ties.buy_surplus {
        ties.lowest
    } else if let Some((_, price, side)) = ties.nearest {
        (price, side)
    } else {
        ties.lowest
    };
    UncrossSummary {
        price: Some(price),
        volume: ties.volume,
        imbalance_side: side,
        imbalance: ties.surplus,
    }
}

/// The side with more quantity, or `None` when demand equals supply.
const fn surplus_side(demand: Qty, supply: Qty) -> Option<Side> {
    if demand > supply {
        Some(Side::Buy)
    } else if supply > demand {
        Some(Side::Sell)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        bids: &[(Price, Qty)],
        asks: &[(Price, Qty)],
        market: (Qty, Qty),
        reference: Option<Price>,
    ) -> UncrossSummary {
        determine(
            bids.iter().copied(),
            asks.iter().copied(),
            market.0,
            market.1,
            reference,
        )
    }

    fn cleared(price: Price, volume: Qty, side: Option<Side>, imbalance: Qty) -> UncrossSummary {
        UncrossSummary {
            price: Some(price),
            volume,
            imbalance_side: side,
            imbalance,
        }
    }

    #[test]
    fn each_rule_decides_in_turn() {
        // 1. Volume: 8 lots can trade at 101, fewer anywhere else.
        assert_eq!(
            run(
                &[(100, 4), (101, 4), (102, 4)],
                &[(100, 4), (101, 4)],
                (0, 0),
                None
            ),
            cleared(101, 8, None, 0)
        );
        // 2. Surplus: 5 lots at 100 (surplus 3 buy) and at 102 (surplus 4 sell).
        assert_eq!(
            run(&[(100, 3), (102, 5)], &[(100, 5), (102, 4)], (0, 0), None),
            cleared(100, 5, Some(Side::Buy), 3)
        );
        // 3. Market pressure: a buy surplus of 3 at both 100 and 101 takes the higher price,
        // a sell surplus the lower.
        assert_eq!(
            run(&[(101, 8)], &[(100, 5)], (0, 0), Some(50)),
            cleared(101, 5, Some(Side::Buy), 3)
        );
        assert_eq!(
            run(&[(101, 5)], &[(100, 8)], (0, 0), Some(500)),
            cleared(100, 5, Some(Side::Sell), 3)
        );
        // 4. Reference price: no surplus at 100 or 101, so the one nearer the reference.
        let balanced = |reference| run(&[(101, 5)], &[(100, 5)], (0, 0), reference);
        assert_eq!(balanced(Some(90)), cleared(100, 5, None, 0));
        assert_eq!(balanced(Some(101)), cleared(101, 5, None, 0));
        // 5. Lowest price: no reference, or a reference exactly halfway.
        assert_eq!(balanced(None), cleared(100, 5, None, 0));
        let halfway = run(&[(102, 5)], &[(100, 5)], (0, 0), Some(101));
        assert_eq!(halfway, cleared(100, 5, None, 0));
        // Mixed surplus sides also fall through to the reference price.
        let mixed = |reference| {
            run(
                &[(100, 3), (102, 5)],
                &[(100, 5), (102, 3)],
                (0, 0),
                reference,
            )
        };
        assert_eq!(mixed(Some(102)), cleared(102, 5, Some(Side::Sell), 3));
        assert_eq!(mixed(Some(99)), cleared(100, 5, Some(Side::Buy), 3));
    }

    #[test]
    fn market_orders_count_at_every_price() {
        // A market buy of 6 against asks at 100 and 103: all 6 trade at 103 only.
        assert_eq!(
            run(&[], &[(100, 2), (103, 4)], (6, 0), None),
            cleared(103, 6, None, 0)
        );
        // Market orders alone clear at the reference price, or not at all.
        assert_eq!(
            run(&[], &[], (7, 4), Some(99)),
            cleared(99, 4, Some(Side::Buy), 3)
        );
        assert_eq!(run(&[], &[], (7, 4), None), UncrossSummary::default());
        assert_eq!(run(&[], &[], (7, 0), Some(99)), UncrossSummary::default());
        assert_eq!(run(&[], &[], (0, 7), Some(99)), UncrossSummary::default());
    }

    #[test]
    fn a_book_that_does_not_cross_has_no_price() {
        assert_eq!(
            run(&[(99, 5), (100, 2)], &[(101, 3)], (0, 0), Some(100)),
            UncrossSummary::default()
        );
        assert_eq!(
            run(&[(99, 5)], &[], (0, 0), None),
            UncrossSummary::default()
        );
        assert_eq!(run(&[], &[], (0, 0), Some(5)), UncrossSummary::default());
    }
}
